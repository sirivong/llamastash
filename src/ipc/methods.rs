//! Method dispatch for the daemon's IPC layer.
//!
//! Unit 2 shipped `ping` / `version` / `shutdown` and `list_models`.
//! Unit 5 added the supervisor-touching methods: `status`,
//! `stop_model`, `stop_all`, `logs_tail`, `start_model`, plus the
//! state-store CRUD surfaces `presets_*` and `favorite_*`. Keeping
//! the registry as a `match` (rather than a `HashMap<&str, fn>`)
//! avoids dynamic-dispatch plumbing for what is, in practice, a
//! small fixed set of methods.

use std::{
  ffi::OsString,
  path::PathBuf,
  sync::{atomic::Ordering, Arc},
  time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{Mutex, RwLock};

use super::protocol::{ErrorCode, ErrorObject, Request, Response, JSONRPC_VERSION};
use crate::backend::identity::ModelIdentity;
use crate::backend::{Backend, LaunchPlan};
use crate::config::loader::{LemonadeConfig, PortRange};
use crate::config::{KnobValue, KnobValueOpt};
use crate::daemon::host_metrics::{HostMetricsSnapshot, SamplerHandles};
use crate::daemon::orphans::ExternalProcess;
use crate::daemon::probe::ProbeOptions;
use crate::daemon::registry::{LaunchId, SupervisorRegistry};
use crate::daemon::shutdown::ShutdownToken;
use crate::daemon::state_store::{self, DaemonState, RunningSnapshot};
use crate::daemon::supervisor::{
  spawn as supervisor_spawn, ManagedModel, ManagedSpawn, ManagedState,
};
use crate::discovery::ModelCatalog;
use crate::gguf::header::{read_path as read_gguf_header, HeaderReadOptions};
use crate::gguf::identity::{compute as compute_model_id, ModelId};
use crate::gpu::GpuInfo;
use crate::launch::favorites::FavoriteEntry;
use crate::launch::mode::LaunchMode;
use crate::launch::params::LaunchParams;
use crate::launch::presets::NamedPreset;

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
  pub active_connections: Arc<std::sync::atomic::AtomicUsize>,
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
  /// Persisted favorites / presets / last_params / running snapshots.
  /// `start_model`, `presets_*`, and `favorite_*` mutate it and
  /// flush to `state.json` after each change.
  pub state: PersistedState,
  /// Inputs the supervisor needs at launch time — binary path, port
  /// range, log directory, probe tuning. Optional because catalog-only
  /// IPC tests don't need to launch anything.
  pub launch: Option<LaunchEnv>,
  /// Snapshot of `llama-server` processes the daemon does *not*
  /// own. Populated by the orphan sweep at startup so `status`
  /// surfaces them read-only (plan: External rows). Wrapped in
  /// `RwLock` so a periodic re-sweep can refresh the slot without
  /// rebuilding the context.
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
  /// Pre-spawn memory admission ledger (R4). Shared across every launch
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
  /// `(arch, gpu_backend)` table. `start_model_handler` lands these
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
  /// Seed mode for knobs no layer filled (R1). Sourced from
  /// `Config.default_launch_mode` (+ `LLAMASTASH_DEFAULT_LAUNCH_MODE`).
  pub default_launch_mode: crate::config::DefaultLaunchMode,
  /// `--fit-ctx` floor (R7). Consumed by `compose` in U6; carried here
  /// so the launch path reads one resolved value. Validated upstream.
  pub fit_ctx_floor: u32,
  /// Strict-fit mode (R19). Consumed by the admission/strict path in
  /// U8; carried here so it rides the same launch env.
  pub strict_fit: bool,
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
      active_connections: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
      catalog,
      supervisors: SupervisorRegistry::new(),
      gpu: Arc::new(GpuInfo::CpuOnly),
      gpu_live: None,
      host_metrics: None,
      state: PersistedState::ephemeral(),
      launch: None,
      external: Arc::new(RwLock::new(Vec::new())),
      proxy_status: None,
      ipc_url: None,
      lemonade: LemonadeConfig::default(),
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

  /// Builder helper: attach the launch environment. `start_model`
  /// requires this to be set; without it the handler returns an
  /// `InvalidRequest`.
  pub fn with_launch_env(mut self, env: LaunchEnv) -> Self {
    self.launch = Some(env);
    self
  }

  /// Builder helper: attach the proxy listener's status cell. The
  /// IPC `status` handler reads from this to surface the `proxy`
  /// block (Unit 5). Catalog-only tests skip this — the response
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
}

/// Top-level dispatch. Always returns a `Response` — protocol violations
/// surface as JSON-RPC error responses rather than disconnects.
pub async fn dispatch_request(ctx: &MethodContext, req: Request) -> Response {
  let id = req.id.clone().unwrap_or(Value::Null);

  if req.jsonrpc != JSONRPC_VERSION {
    return Response::err(
      id,
      ErrorObject::new(
        ErrorCode::InvalidRequest,
        format!("jsonrpc must be \"{JSONRPC_VERSION}\""),
      ),
    );
  }

  match req.method.as_str() {
    "ping" => Response::ok(id, json!("pong")),
    "version" => {
      let uptime_secs = ctx.started_at.elapsed().as_secs();
      let connections = ctx.active_connections.load(Ordering::Relaxed);
      Response::ok(
        id,
        json!({
          "name": env!("CARGO_PKG_NAME"),
          "version": env!("CARGO_PKG_VERSION"),
          // Wire protocol version. Bumped only when an existing
          // method's request or response shape changes in a way
          // older clients can't parse. New methods are additive
          // and don't require a bump; callers can feature-detect
          // via `capabilities`.
          "protocol_version": 1u32,
          "pid": std::process::id(),
          "uptime_seconds": uptime_secs,
          "connections": connections,
        }),
      )
    }
    "capabilities" => {
      // Method-set introspection. Returned as a sorted array of the
      // method names this daemon advertises so clients can do a
      // cheap feature-detect before issuing an unknown method call.
      let methods = supported_methods();
      Response::ok(
        id,
        json!({
          "protocol_version": 1u32,
          "methods": methods,
        }),
      )
    }
    "shutdown" => {
      ctx.shutdown.trigger();
      Response::ok(id, json!({"shutdown": "scheduled"}))
    }
    #[cfg(feature = "test-fixtures")]
    "_test_sleep" => {
      // Test-only seam: holds the connection open for the requested
      // number of milliseconds. Used by drain-timeout tests to model
      // a slow in-flight request. Behind the `test-fixtures` feature
      // so production builds never expose it.
      let ms: u64 = req
        .params
        .as_ref()
        .and_then(|p| p.get("ms"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
      tokio::time::sleep(Duration::from_millis(ms)).await;
      Response::ok(id, json!({"slept_ms": ms}))
    }
    "list_models" => {
      let body = ctx.catalog.to_list_response().await;
      Response::ok(id, body)
    }
    "status" => Response::ok(id, status_response(ctx).await),
    "start_model" => respond(id, start_model_handler(ctx, req.params).await),
    "stop_model" => respond(id, stop_model_handler(ctx, req.params).await),
    "stop_all" => respond(id, stop_all_handler(ctx, req.params).await),
    "stop_external" => respond(id, stop_external_handler(ctx, req.params).await),
    "logs_tail" => respond(id, logs_tail_handler(ctx, req.params).await),
    "presets_list" => respond(id, presets_list_handler(ctx, req.params).await),
    "presets_save" => respond(id, presets_save_handler(ctx, req.params).await),
    "presets_delete" => respond(id, presets_delete_handler(ctx, req.params).await),
    "presets_show" => respond(id, presets_show_handler(ctx, req.params).await),
    "favorite_add" => respond(id, favorite_add_handler(ctx, req.params).await),
    "favorite_remove" => respond(id, favorite_remove_handler(ctx, req.params).await),
    "favorite_list" => respond(id, favorite_list_handler(ctx).await),
    "last_params_list" => respond(id, last_params_list_handler(ctx).await),
    other => Response::err(
      id,
      ErrorObject::new(
        ErrorCode::MethodNotFound,
        format!("unknown method: {other}"),
      ),
    ),
  }
}

/// Lift a `Result<Value, ErrorObject>` into a `Response`. Collapses the
/// 14 near-identical `match { Ok(v) => Response::ok(id, v), Err(e) =>
/// Response::err(id, e) }` arms in the dispatcher.
fn respond(id: Value, result: Result<Value, ErrorObject>) -> Response {
  match result {
    Ok(v) => Response::ok(id, v),
    Err(e) => Response::err(id, e),
  }
}

/// Snapshot every active managed model plus the daemon's GPU info.
/// `status` is read-only; never triggers any state-machine transitions.
async fn status_response(ctx: &MethodContext) -> Value {
  let snap = ctx.supervisors.snapshot().await;
  // Post-launch actuals (R6) live on the persisted running snapshot
  // (stamped by the recorder on Ready); the live status row is built
  // from the supervisor, so cross-reference by (id, port) to surface
  // the resolved context.
  let running = ctx.state.snapshot().await.running;
  let mut models: Vec<Value> = Vec::with_capacity(snap.len());
  for (launch_id, model) in snap {
    let state = model.state().await;
    let pid = model.pid().await;
    let ready_at = model.ready_at().await;
    // Wrap `ManagedState` in a small `{state, cause?}` object
    // (P2-16). The legacy nested `{"state": {"state": "ready"}}`
    // shape was a serde default; the new shape is `"state": {
    // "state": "ready" }` — same as before for `state.state`
    // (preserving existing pinned parsers) but `Error{cause}` now
    // surfaces the cause as a sibling string field instead of being
    // hidden in serde tagged-enum content.
    let state_obj = match state.cause() {
      Some(cause) => json!({"state": state.label(), "cause": cause}),
      None => json!({"state": state.label()}),
    };
    // `params` so an agent can reproduce the launch without a
    // separate `last_params_list` call.
    let params = model.params();
    let params_json = json!({
      "model_path": params.model_path,
      "mode": model.mode().label(),
      "ctx": params.ctx,
      "port": params.port,
      "reasoning": params.reasoning,
      "knobs": &params.knobs,
      "extras": params
        .extras
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect::<Vec<_>>(),
    });
    let latest = model.latest_resource().await;
    let latest_rss_bytes = latest.as_ref().map(|r| r.rss_bytes);
    let latest_cpu_pct = latest.as_ref().map(|r| r.cpu_percent);
    let resolved_ctx = running
      .iter()
      .find(|r| r.port == model.port())
      .and_then(|r| r.actuals.resolved_ctx);
    let row = json!({
      "launch_id": launch_id,
      "id": model.id(),
      "port": model.port(),
      "mode": model.mode().label(),
      "pid": pid,
      "ready_at": ready_at,
      "state": state_obj,
      "params": params_json,
      "latest_rss_bytes": latest_rss_bytes,
      "latest_cpu_pct": latest_cpu_pct,
      // Resolved context window `--fit` chose (R6); null until the
      // post-Ready `/props` fetch lands or when the build omits it.
      "resolved_ctx": resolved_ctx,
    });
    models.push(row);
  }
  // Delegated Lemonade models — the registry holds only the shared
  // umbrella (one row whose path is the `lemond` binary), but every
  // model made resident via `start_model` persisted a RunningSnapshot
  // at the umbrella's port. Project each as a first-class row: the
  // synthetic `lemonade://<name>` path matches the catalog entry, so
  // the TUI list pane and `llamastash list` show the *model* as
  // running, not just the umbrella. State comes from the preload
  // task's recorded outcome (`Loading` / `Ready` / `Error{cause}`);
  // a snapshot with no recorded outcome (re-adopted across a daemon
  // restart) falls back to mirroring the umbrella. Rows are emitted
  // only while the umbrella is registered; with it gone the
  // snapshots are unreachable leftovers (the boot sweep reaps them).
  if let Some(umbrella) = ctx
    .supervisors
    .get(&crate::backend::lemonade::umbrella_launch_id())
    .await
  {
    let ustate_obj = flatten_state(&umbrella.state().await);
    let upid = umbrella.pid().await;
    for running_snap in ctx.state.snapshot().await.running.iter() {
      let Some(backend_id) = lemonade_snapshot_id(running_snap) else {
        continue;
      };
      let state_obj = match ctx.supervisors.delegated_state(&backend_id.name).await {
        Some(s) => flatten_state(&s),
        None => ustate_obj.clone(),
      };
      let synthetic_id = crate::gguf::identity::ModelId {
        path: running_snap.params.model_path.clone(),
        header_blake3: [0u8; 32],
      };
      let params_json = json!({
        "model_path": running_snap.params.model_path,
        "mode": running_snap.params.mode.label(),
        "ctx": running_snap.params.ctx,
        "port": running_snap.params.port,
        "reasoning": running_snap.params.reasoning,
        "knobs": &running_snap.params.knobs,
        "extras": running_snap.params.extras
          .iter()
          .map(|s| s.to_string_lossy().into_owned())
          .collect::<Vec<_>>(),
      });
      models.push(json!({
        "launch_id": crate::backend::lemonade::delegated_launch_id(&backend_id.name),
        "id": synthetic_id,
        "port": running_snap.port,
        "mode": running_snap.params.mode.label(),
        "pid": upid,
        "ready_at": running_snap.started_at,
        "state": state_obj,
        "params": params_json,
        // Resource readings stay on the umbrella's own row — mirroring
        // its RSS onto every resident model would double-count it.
        "latest_rss_bytes": Value::Null,
        "latest_cpu_pct": Value::Null,
      }));
    }
  }
  // External — read-only rows for `llama-server` processes the
  // daemon doesn't own. Populated by the startup orphan sweep;
  // mirrors the plan's "External read-only" surface (plan: list-
  // pane glyph `⇪`). Stable shape: `{pid, cmdline, model_path}`.
  let external_snapshot = ctx.external.read().await.clone();
  let external: Vec<Value> = external_snapshot
    .iter()
    .map(|e| {
      json!({
        "pid": e.pid,
        "cmdline": e.cmdline,
        "model_path": e.model_path,
        // Tier-A orphan-tracking fields. `port` lets agents diff the
        // wire snapshot against `ss`/`lsof` without parsing argv;
        // `launched_by_llamastash` exposes whether the orphan carries
        // the supervisor's spawn marker so operators can spot
        // sibling-instance orphans at a glance.
        "port": e.port,
        "launched_by_llamastash": e.launched_by_llamastash,
      })
    })
    .collect();
  // Host-level metrics (CPU%, RAM, GPU util/temp/VRAM aggregates).
  // Sampled by the daemon's `host_metrics` task at 1 Hz. When no
  // sampler is attached (catalog-only contexts), emit a default
  // snapshot rather than `null` so clients see a stable object
  // shape — `gpu_backend == "unsampled"` already distinguishes the
  // never-sampled case from a real reading.
  //
  // Serialize the snapshot directly under the read lock instead of
  // cloning it out first; `HostMetricsSnapshot` already implements
  // `Serialize` for `&Self`, so this saves one full struct clone
  // (including the `gpu_backend: String`) per status call.
  let host = match &ctx.host_metrics {
    Some(slot) => {
      let host_snap = slot.read().await;
      serde_json::to_value(&*host_snap).unwrap_or(Value::Null)
    }
    None => {
      let default_snap = HostMetricsSnapshot {
        gpu_backend: HostMetricsSnapshot::UNINITIALIZED_BACKEND.into(),
        ..HostMetricsSnapshot::default()
      };
      serde_json::to_value(default_snap).unwrap_or(Value::Null)
    }
  };
  // Prefer the live GpuInfo cell when the sampler is attached so
  // `status.gpu` follows hotplug / late-driver-load events. Falls
  // back to the boot-time `ctx.gpu` snapshot when the sampler is
  // off (catalog-only tests).
  let gpu = match &ctx.gpu_live {
    Some(slot) => serde_json::to_value(&*slot.read().await).unwrap_or(Value::Null),
    None => serde_json::to_value(ctx.gpu.as_ref()).unwrap_or(Value::Null),
  };
  // Proxy block — read-only projection of the listener's shared
  // status cell. Catalog-only tests that never bring the proxy up
  // leave `proxy_status` as `None`; the field is omitted in that
  // case so pre-Unit-5 fixtures stay byte-identical. The wire shape
  // is locked by the plan's Key Decision row on `proxy.status`:
  //
  // ```
  // "proxy": {
  //   "enabled": bool,
  //   "listen": "127.0.0.1:11434" | null,
  //   "status": "disabled" | "listening" | "port_in_use" | "unbound",
  //   "bind_error": "permission denied" | null,
  // }
  // ```
  let proxy = ctx.proxy_status.as_ref().map(project_proxy_status);
  // Launch device catalog — the exact `--device` selectors the TUI
  // picker may offer, each tagged with the binary that owns it.
  // Sourced from every configured binary's `--list-devices` (not from
  // vendor tools), so what the picker shows is precisely what
  // `llama-server` will accept. Empty when no binary is configured.
  let device_catalog = match ctx.launch.as_ref() {
    Some(env) => {
      let catalog_snap = env.device_catalog.read().await;
      serde_json::to_value(&*catalog_snap).unwrap_or(Value::Null)
    }
    None => Value::Null,
  };
  let backends = backends_status(ctx).await;
  let mut body = json!({
    "models": models,
    "external": external,
    "gpu": gpu,
    "host": host,
    "device_catalog": device_catalog,
    "backends": backends,
    "daemon": {
      "pid": std::process::id(),
      "uptime_seconds": ctx.started_at.elapsed().as_secs(),
      "active_connections": ctx.active_connections.load(Ordering::Relaxed),
      "build": env!("CARGO_PKG_VERSION"),
      "server_path": ctx
        .launch
        .as_ref()
        .map(|env| env.binary.display().to_string()),
      "ipc_url": ctx.ipc_url,
    },
  });
  if let Some(proxy) = proxy {
    if let Some(obj) = body.as_object_mut() {
      obj.insert("proxy".into(), proxy);
    }
  }
  body
}

/// Build the `status.backends` array (R3/R16): one row per backend with
/// whether its binary is installed on this host and which accelerators it
/// can run on. llama.cpp's accelerator set unions its CPU floor with the
/// GPU backends the live device catalog reveals; Lemonade reports its
/// static cpu+npu (a live `lemond` system-info probe is deferred).
async fn backends_status(ctx: &MethodContext) -> Value {
  use crate::backend::lemonade::LemonadeBackend;
  use crate::backend::llama_cpp::LlamaCppBackend;
  use crate::backend::Backend;

  let llama = LlamaCppBackend::new();
  let llama_installed = ctx
    .launch
    .as_ref()
    .map(|e| e.binary.exists())
    .unwrap_or(false);
  let mut llama_acc = llama.accelerators();
  if let Some(env) = ctx.launch.as_ref() {
    let cat = env.device_catalog.read().await;
    for d in cat.iter() {
      if let Some(a) = accelerator_from_selector(&d.selector) {
        llama_acc.insert(a);
      }
    }
  }

  let lemonade = LemonadeBackend::new();
  let mut lemonade_row = backend_row(
    lemonade.id(),
    lemonade.lifecycle().label(),
    lemond_installed(&ctx.lemonade),
    lemonade.accelerators().labels(),
  );
  // Managed-multiplexer health: surface whether the umbrella llamastash
  // supervises is actually up, so `status` distinguishes "installed" (binary on
  // disk) from "running" (umbrella Ready). When enabled but down — e.g. the
  // configured port was already taken at boot — this reads `not running`, which
  // is the only visible signal outside the daemon log.
  if let Some(obj) = lemonade_row.as_object_mut() {
    obj.insert("enabled".into(), json!(ctx.lemonade.enabled));
    obj.insert("umbrella".into(), json!(lemonade_umbrella_state(ctx).await));
  }
  // Each row carries its resolved `binary` path when one exists, so
  // clients (the TUI's Daemon panel server row) can list every enabled
  // backend's executable generically — no per-backend client code.
  let mut llama_row = backend_row(
    llama.id(),
    llama.lifecycle().label(),
    llama_installed,
    llama_acc.labels(),
  );
  let llama_binary = ctx.launch.as_ref().map(|e| e.binary.display().to_string());
  set_backend_binary(&mut llama_row, llama_binary);
  let lemonade_binary =
    crate::backend::lemonade::resolve_lemond_binary(&ctx.lemonade).map(|b| b.display().to_string());
  set_backend_binary(&mut lemonade_row, lemonade_binary);
  json!([llama_row, lemonade_row])
}

/// Attach a backend row's resolved `binary` path; absent when no binary
/// resolves so clients can tell "backend known" from "executable found".
fn set_backend_binary(row: &mut Value, binary: Option<String>) {
  if let (Some(obj), Some(bin)) = (row.as_object_mut(), binary) {
    obj.insert("binary".into(), json!(bin));
  }
}

/// The managed `lemond` umbrella's state for `status`, distinct from the
/// `installed` (binary-resolvable) signal. `disabled` when the backend is off;
/// otherwise reflects the supervised umbrella: `running` (Ready), `starting`
/// (spawned, probing), or `not running` (never came up / exited — commonly a
/// boot-time port conflict).
async fn lemonade_umbrella_state(ctx: &MethodContext) -> &'static str {
  use crate::daemon::supervisor::ManagedState;
  if !ctx.lemonade.enabled {
    return "disabled";
  }
  match ctx
    .supervisors
    .get(&crate::backend::lemonade::umbrella_launch_id())
    .await
  {
    Some(m) => match m.state().await {
      ManagedState::Ready => "running",
      ManagedState::Launching | ManagedState::Loading => "starting",
      ManagedState::Error { .. } | ManagedState::Stopping | ManagedState::Stopped => "not running",
    },
    None => "not running",
  }
}

fn backend_row(id: &str, lifecycle: &str, installed: bool, accelerators: Vec<&str>) -> Value {
  json!({
    "id": id,
    "lifecycle": lifecycle,
    "installed": installed,
    "accelerators": accelerators,
  })
}

/// Map a llama.cpp `--device` selector prefix to an accelerator class.
fn accelerator_from_selector(selector: &str) -> Option<crate::backend::Accelerator> {
  use crate::backend::Accelerator;
  let s = selector.to_ascii_lowercase();
  if s.starts_with("cuda") {
    Some(Accelerator::Cuda)
  } else if s.starts_with("rocm") {
    Some(Accelerator::Rocm)
  } else if s.starts_with("vulkan") {
    Some(Accelerator::Vulkan)
  } else if s.starts_with("metal") {
    Some(Accelerator::Metal)
  } else {
    None
  }
}

/// The "installed" signal for the Lemonade backend in `status`. Honors the
/// full resolution order — explicit `lemonade.binary` first, then `lemond` /
/// `lemonade` on PATH — so a user who points at an off-PATH `lemond` still
/// reads as installed.
fn lemond_installed(cfg: &LemonadeConfig) -> bool {
  crate::backend::lemonade::resolve_lemond_binary(cfg).is_some()
}

/// Project the proxy listener's status cell into the wire shape
/// surfaced under `status.proxy`. The cell is the single source of
/// truth — Unit 1 wired the listener task to write every transition
/// (Disabled / Listening / PortInUse / Unbound) here; Unit 5 is the
/// read side.
///
/// `listen` is the *attempted* address: `Disabled` emits `null`
/// (no bind was attempted), every other variant carries the address
/// the daemon tried to bind. `bind_error` is non-null only when the
/// variant is `Unbound` — `PortInUse` is its own discriminator and
/// callers shouldn't need a parallel string to recognise it.
fn project_proxy_status(cell: &crate::proxy::StatusCell) -> Value {
  use crate::proxy::ProxyStatus;
  let snapshot = cell.read().unwrap_or_else(|e| e.into_inner()).clone();
  match snapshot {
    ProxyStatus::Disabled => json!({
      "enabled": false,
      "listen": Value::Null,
      "host": Value::Null,
      "status": "disabled",
      "auth": "none",
      "bind_error": Value::Null,
    }),
    ProxyStatus::Listening {
      addr,
      auth_enforced,
    } => json!({
      "enabled": true,
      "listen": addr.to_string(),
      "host": addr.ip().to_string(),
      "status": "listening",
      "auth": if auth_enforced { "enforced" } else { "none" },
      "bind_error": Value::Null,
    }),
    ProxyStatus::PortInUse { addr } => json!({
      "enabled": true,
      "listen": addr.to_string(),
      "host": addr.ip().to_string(),
      "status": "port_in_use",
      "auth": "none",
      "bind_error": Value::Null,
    }),
    ProxyStatus::Unbound { addr, bind_error } => json!({
      "enabled": true,
      "listen": addr.to_string(),
      "host": addr.ip().to_string(),
      "status": "unbound",
      "auth": "none",
      "bind_error": bind_error,
    }),
    // Refused to expose a non-loopback proxy without auth. The daemon
    // is healthy; the proxy just didn't bind. `auth` reports
    // `"required"` so a client can distinguish this from a plain bind
    // failure and surface the fix.
    ProxyStatus::RefusedInsecure { addr } => json!({
      "enabled": true,
      "listen": addr.to_string(),
      "host": addr.ip().to_string(),
      "status": "refused_insecure",
      "auth": "required",
      "bind_error":
        "refused to bind a non-loopback proxy without authentication; \
         set proxy.api_key or pass --insecure-no-auth",
    }),
  }
}

#[derive(Deserialize)]
struct StopParams {
  launch_id: LaunchId,
  #[serde(default = "default_grace_secs")]
  grace_secs: u64,
}

fn default_grace_secs() -> u64 {
  5
}

/// Upper bound on the SIGTERM→SIGKILL grace window. Caps both
/// managed `stop_model` and external `stop_external`. Keeps
/// `Duration::from_secs(grace)` arithmetic safe and prevents a
/// same-UID caller from holding the IPC task open indefinitely by
/// passing `u64::MAX`.
const MAX_GRACE_SECS: u64 = 300;

fn check_grace_secs(secs: u64) -> Result<(), ErrorObject> {
  if secs > MAX_GRACE_SECS {
    return Err(ErrorObject::new(
      ErrorCode::InvalidParams,
      format!("grace_secs={secs} exceeds maximum {MAX_GRACE_SECS}; clamp client-side"),
    ));
  }
  Ok(())
}

async fn stop_model_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: StopParams = parse_params(params)?;
  check_grace_secs(parsed.grace_secs)?;
  // A delegated Lemonade model is not a supervised child — "stopping" it
  // means unloading it from the shared umbrella, which keeps running.
  if let Some(name) = crate::backend::lemonade::delegated_model_name(parsed.launch_id.as_str()) {
    return stop_delegated_lemonade(ctx, &parsed.launch_id, name).await;
  }
  let model = ctx
    .supervisors
    .get(&parsed.launch_id)
    .await
    .ok_or_else(|| {
      ErrorObject::new(
        ErrorCode::InvalidParams,
        format!("unknown launch_id: {}", parsed.launch_id.as_str()),
      )
    })?;
  let stopped_port = model.port();
  let final_state = model.stop(Duration::from_secs(parsed.grace_secs)).await;
  ctx.supervisors.remove(&parsed.launch_id).await;
  // Drop the running snapshot keyed by `(id, port)` so a second
  // launch of the same GGUF on a different port keeps its row.
  let stopped_id: ModelIdentity = model.id().clone().into();
  let stopped_umbrella = parsed.launch_id == crate::backend::lemonade::umbrella_launch_id();
  ctx
    .state
    .mutate(|s| {
      s.running
        .retain(|r| !(r.id == stopped_id && r.port == stopped_port));
      // Stopping the umbrella takes every delegated model down with it —
      // their snapshots would otherwise linger as ghost rows the next
      // `ensure_umbrella` (fresh process, nothing resident) can't honor.
      if stopped_umbrella {
        s.running.retain(|r| lemonade_snapshot_id(r).is_none());
      }
    })
    .await;
  if stopped_umbrella {
    ctx.supervisors.clear_delegated().await;
  }
  Ok(json!({
    "launch_id": parsed.launch_id,
    "state": flatten_state(&final_state),
  }))
}

/// Stop one delegated Lemonade model: best-effort unload from the shared
/// umbrella, then drop its running snapshot so `status` stops emitting the
/// row. The umbrella itself keeps running (stop it via its own
/// `lemonade-umbrella` launch id). An unload refusal is logged but doesn't
/// fail the stop — the snapshot is the daemon's own bookkeeping, and a
/// model the umbrella already evicted should always be clearable.
async fn stop_delegated_lemonade(
  ctx: &MethodContext,
  launch_id: &LaunchId,
  name: &str,
) -> Result<Value, ErrorObject> {
  if let Some(umbrella) = ctx
    .supervisors
    .get(&crate::backend::lemonade::umbrella_launch_id())
    .await
  {
    match crate::backend::lemonade::LemonadeClient::new(umbrella.port()) {
      Ok(client) => {
        if let Err(e) = client.unload(name).await {
          log::warn!("lemonade: unload of `{name}` failed (dropping the row anyway): {e}");
        }
      }
      Err(e) => log::warn!("lemonade: could not build client to unload `{name}`: {e}"),
    }
  }
  ctx.supervisors.remove_delegated(name).await;
  let removed = ctx
    .state
    .mutate(|s| {
      let before = s.running.len();
      s.running
        .retain(|r| lemonade_snapshot_id(r).map(|b| b.name.as_str()) != Some(name));
      before != s.running.len()
    })
    .await;
  if !removed {
    return Err(ErrorObject::new(
      ErrorCode::InvalidParams,
      format!("unknown launch_id: {}", launch_id.as_str()),
    ));
  }
  Ok(json!({
    "launch_id": launch_id,
    "state": flatten_state(&ManagedState::Stopped),
  }))
}

/// The lemonade [`BackendModelId`](crate::backend::identity::BackendModelId)
/// behind a running snapshot, or `None` for any other identity (GGUF, other
/// backends). The one predicate shared by the `status` projection and both
/// `stop_model` snapshot sweeps, so "is this a delegated lemonade row" can't
/// drift between them.
pub(crate) fn lemonade_snapshot_id(
  snap: &crate::daemon::state_store::RunningSnapshot,
) -> Option<&crate::backend::identity::BackendModelId> {
  snap
    .id
    .as_backend()
    .filter(|b| b.backend == crate::backend::lemonade::LEMONADE_BACKEND_ID)
}

/// Flatten `ManagedState` to a JSON object whose `state` field is a
/// lowercase string label plus an optional `cause`. Used by
/// `stop_model` and `stop_all` responses so the shape matches the
/// `status` rows (P2-16) and the legacy nested-enum form is gone.
fn flatten_state(state: &ManagedState) -> Value {
  match state.cause() {
    Some(cause) => json!({"state": state.label(), "cause": cause}),
    None => json!({"state": state.label()}),
  }
}

#[derive(Deserialize)]
struct StopExternalParams {
  pid: u32,
  /// Grace seconds between SIGTERM and SIGKILL. Mirrors
  /// [`StopParams::grace_secs`] for parity with managed stop.
  #[serde(default = "default_grace_secs")]
  grace_secs: u64,
}

/// Stop an unmanaged `llama-server` process the daemon previously
/// surfaced via the `external` snapshot. Sends SIGTERM, waits up
/// to `grace_secs`, then SIGKILL if the process is still alive.
/// The external snapshot is rebuilt next time `status` is fetched
/// (the supervisor doesn't drive sysinfo on a tick), so the row
/// will keep appearing until the next sweep refreshes it.
async fn stop_external_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: StopExternalParams = parse_params(params)?;
  check_grace_secs(parsed.grace_secs)?;
  // Confirm the PID is one we surfaced as external and snapshot
  // its recorded start_time. We later re-verify the live
  // start_time matches before each signal to defend against PID
  // recycling: if the original process exits during the grace
  // window and the kernel hands the pid to an unrelated process,
  // its start_time will differ from our snapshot and we refuse to
  // signal it.
  let recorded_start_time = {
    let known = ctx
      .external
      .read()
      .await
      .iter()
      .find(|e| e.pid == parsed.pid)
      .map(|e| e.start_time_secs);
    match known {
      Some(s) => s,
      None => {
        return Err(ErrorObject::new(
          ErrorCode::InvalidParams,
          format!("pid {} is not a known external llama-server", parsed.pid),
        ))
      }
    }
  };
  // Bound the pid cast: a u32 > i32::MAX flips negative under
  // `as i32` and `libc::kill(neg, sig)` would signal a process
  // group. Kernel pid_max on every supported platform is well below
  // i32::MAX in practice, but the daemon shouldn't trust that.
  if parsed.pid > i32::MAX as u32 {
    return Err(ErrorObject::new(
      ErrorCode::InvalidParams,
      format!("pid {} exceeds i32::MAX; refusing to signal", parsed.pid),
    ));
  }

  // Helper: returns Some(true) if alive AND start_time matches, Some(false)
  // if alive but pid has been reused, None if dead. We sample via
  // `sysinfo` rather than `kill(pid, 0)` so we can compare start_time
  // — the cheap liveness check alone can't distinguish recycle.
  //
  // Defensive: if either the live or expected `start_time` is 0 we
  // can't *prove* identity (sysinfo can hand back 0 on some platforms /
  // for kernel processes, and adopted-but-already-dead entries are
  // seeded with `start_time_secs = 0` in `daemon::mod`). Treat that
  // as a mismatch — refusing to signal is the safe failure mode.
  //
  // Off-thread via `spawn_blocking`: sysinfo does synchronous /proc
  // I/O (Linux) or sysctl (macOS) per refresh. In the 100ms grace
  // loop that's ~50 calls per stop, and `stop_all` runs them in
  // parallel via `join_all` — left on the async worker, a fleet of
  // concurrent stops can saturate every reactor thread and stall
  // probe polling for a launching model.
  async fn live_and_same(pid: u32, expected_start: u64) -> Option<bool> {
    tokio::task::spawn_blocking(move || {
      use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};
      let refresh = ProcessRefreshKind::everything();
      let mut sys = System::new_with_specifics(RefreshKind::nothing().with_processes(refresh));
      sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        refresh,
      );
      sys.process(Pid::from_u32(pid)).map(|p| {
        let live = p.start_time();
        live != 0 && expected_start != 0 && live == expected_start
      })
    })
    .await
    .unwrap_or(None)
  }
  match live_and_same(parsed.pid, recorded_start_time).await {
    Some(true) => {}
    Some(false) => {
      ctx.external.write().await.retain(|e| e.pid != parsed.pid);
      return Err(ErrorObject::new(
        ErrorCode::InvalidParams,
        format!(
          "pid {} has been recycled; refusing to signal (start_time mismatch)",
          parsed.pid
        ),
      ));
    }
    None => {
      // Already gone — surface as success.
      ctx.external.write().await.retain(|e| e.pid != parsed.pid);
      return Ok(json!({
        "pid": parsed.pid,
        "killed_with_sigkill": false,
      }));
    }
  }
  // SIGTERM first — give the process time to exit cleanly. Goes
  // through [`ProcessControl`] so Unit 6 picks up the Windows
  // single-pid path without a second migration here.
  use crate::util::process_control::SignalTarget;
  let pc = crate::util::process_control::platform_default();
  pc.signal_graceful(SignalTarget::SinglePid(parsed.pid));
  let grace = Duration::from_secs(parsed.grace_secs);
  let mut elapsed = Duration::ZERO;
  let step = Duration::from_millis(100);
  while elapsed < grace {
    match live_and_same(parsed.pid, recorded_start_time).await {
      Some(true) => {}
      _ => break, // gone, or pid was recycled — either way stop signalling
    }
    tokio::time::sleep(step).await;
    elapsed += step;
  }
  // Final check; SIGKILL only if same process is still up.
  let mut sent_kill = false;
  if matches!(
    live_and_same(parsed.pid, recorded_start_time).await,
    Some(true)
  ) {
    pc.signal_kill(SignalTarget::SinglePid(parsed.pid));
    sent_kill = true;
  }
  ctx.external.write().await.retain(|e| e.pid != parsed.pid);
  Ok(json!({
    "pid": parsed.pid,
    "killed_with_sigkill": sent_kill,
  }))
}

#[derive(Default, Deserialize)]
struct StopAllParams {
  #[serde(default)]
  grace_secs: Option<u64>,
}

async fn stop_all_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  // `stop_all` is the only handler called with `None` params by the
  // TUI's old code path; treat absent / null as an empty options
  // object rather than rejecting at parse time.
  let parsed: StopAllParams = match params {
    Some(Value::Null) | None => StopAllParams::default(),
    other => parse_params(other)?,
  };
  let grace_secs = parsed.grace_secs.unwrap_or_else(default_grace_secs);
  check_grace_secs(grace_secs)?;
  let outcomes = stop_all_managed(ctx, Duration::from_secs(grace_secs)).await;
  let stopped: Vec<Value> = outcomes
    .iter()
    .map(|(launch_id, state)| json!({"launch_id": launch_id, "state": flatten_state(state)}))
    .collect();
  let count = stopped.len();
  Ok(json!({"stopped": stopped, "count": count}))
}

/// SIGTERM-then-SIGKILL every managed launch concurrently, drop them
/// from the registry, and prune `state.running`. Returns the
/// (launch_id, final_state) pairs for callers that need to surface
/// them on the wire.
///
/// Exposed so the daemon's shutdown path can kill its supervised
/// children before `run_foreground` returns. The supervisor spawns
/// `llama-server` with `setsid`, so without this hook a graceful
/// `daemon stop` / SIGINT / IPC `shutdown` leaves the children
/// running as init-owned orphans. R42's orphan adoption only intends
/// to rescue children from *crashes* (SIGKILL, segfault); it should
/// not turn deliberate shutdown into a leak.
///
/// The `join_all` keeps wall-clock equal to the slowest stop rather
/// than the sum — the original sequential loop blew the default IPC
/// client timeout for 2+ stuck launches.
pub(crate) async fn stop_all_managed(
  ctx: &MethodContext,
  grace: Duration,
) -> Vec<(LaunchId, ManagedState)> {
  use futures::future::join_all;
  let snap = ctx.supervisors.snapshot().await;
  let stops = snap.into_iter().map(|(launch_id, model)| async move {
    let final_state = model.stop(grace).await;
    let model_id = model.id().clone();
    let port = model.port();
    (launch_id, model_id, port, final_state)
  });
  let outcomes = join_all(stops).await;

  let mut stopped: Vec<(LaunchId, ManagedState)> = Vec::with_capacity(outcomes.len());
  let mut stopped_keys: Vec<(ModelIdentity, u16)> = Vec::with_capacity(outcomes.len());
  for (launch_id, model_id, port, final_state) in outcomes {
    ctx.supervisors.remove(&launch_id).await;
    stopped_keys.push((model_id.into(), port));
    stopped.push((launch_id, final_state));
  }
  if !stopped_keys.is_empty() {
    ctx
      .state
      .mutate(|s| {
        s.running.retain(|r| {
          !stopped_keys
            .iter()
            .any(|(id, port)| *id == r.id && *port == r.port)
        })
      })
      .await;
  }
  stopped
}

#[derive(Deserialize)]
struct LogsTailParams {
  launch_id: LaunchId,
  #[serde(default = "default_lines")]
  lines: usize,
}

fn default_lines() -> usize {
  200
}

async fn logs_tail_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: LogsTailParams = parse_params(params)?;
  // A delegated Lemonade model has no process of its own — its log *is*
  // the shared umbrella's log, so tail that one.
  let lookup_id = match crate::backend::lemonade::delegated_model_name(parsed.launch_id.as_str()) {
    Some(_) => crate::backend::lemonade::umbrella_launch_id(),
    None => parsed.launch_id.clone(),
  };
  let model = ctx.supervisors.get(&lookup_id).await.ok_or_else(|| {
    ErrorObject::new(
      ErrorCode::InvalidParams,
      format!("unknown launch_id: {}", parsed.launch_id.as_str()),
    )
  })?;
  let tail = model.tail(parsed.lines).await;
  Ok(json!({
    "launch_id": parsed.launch_id,
    "lines": tail,
  }))
}

/// Wire-shape for the `start_model` IPC method. The fields land
/// verbatim from JSON-RPC; `start_model_inner` consumes the parsed
/// struct so the proxy's auto-start path (Unit 4) can build one
/// directly without going through JSON.
#[derive(Deserialize, Default, Clone)]
pub(crate) struct StartParams {
  /// Absolute path to the GGUF the user wants to launch. We compute
  /// the canonical `ModelId` by reading its header on the daemon
  /// side rather than trusting the caller — keeps the surface
  /// minimal for CLI/TUI clients.
  pub(crate) model_path: PathBuf,
  #[serde(default)]
  pub(crate) mode: Option<LaunchModeWire>,
  #[serde(default)]
  pub(crate) ctx: Option<u32>,
  #[serde(default)]
  pub(crate) port: Option<u16>,
  /// Soft port preference — if the supplied port is free at
  /// reservation time, use it; otherwise allocate from the
  /// configured range. Distinct from `port` which is strict and
  /// errors on conflict. The TUI sets this so a returning user
  /// lands on their previously-bound port without scripted clients
  /// silently losing strict semantics.
  #[serde(default)]
  pub(crate) prefer_port: Option<u16>,
  #[serde(default)]
  pub(crate) reasoning: Option<bool>,
  /// Caller-supplied typed knob overrides. Each `Some` field lands
  /// on the `LayerLabel::User` layer of the resolver, outranking
  /// last_used / arch_default / built-in.
  #[serde(default)]
  pub(crate) knobs: crate::config::TypedKnobs,
  /// Free-form argv tail for `llama-server` flags the typed editor
  /// doesn't model. Appended after the resolved knobs.
  #[serde(default)]
  pub(crate) extras: Vec<String>,
  /// Optional path to a multimodal projector (mmproj) file. When
  /// `None`, the daemon auto-detects by scanning the parent
  /// directory of the model for a `mmproj-<stem>.gguf` companion.
  /// Set to `Some(path)` to override the auto-detection, or leave
  /// as `None` to let the daemon find it automatically.
  #[serde(default)]
  pub(crate) mmproj_path: Option<PathBuf>,
  /// Per-model backend override (R17). `None` / `auto` runs the identity
  /// rule (GGUF → llama.cpp, registry → its backend); an explicit value
  /// forces a backend. Set by `start --backend` and the TUI Launch picker.
  #[serde(default)]
  pub(crate) backend: Option<crate::launch::params::BackendChoice>,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LaunchModeWire {
  Chat,
  Embedding,
  Rerank,
}

impl From<LaunchModeWire> for LaunchMode {
  fn from(m: LaunchModeWire) -> Self {
    match m {
      LaunchModeWire::Chat => LaunchMode::Chat,
      LaunchModeWire::Embedding => LaunchMode::Embedding,
      LaunchModeWire::Rerank => LaunchMode::Rerank,
    }
  }
}

/// Sorted list of every method `dispatch_request` knows. Used by
/// the `capabilities` handler so clients can feature-detect. The
/// names here mirror the wire spec in `docs/architecture.md`; a new
/// method must be added in both places.
const PUBLIC_METHODS: &[&str] = &[
  "ping",
  "version",
  "capabilities",
  "shutdown",
  "list_models",
  "status",
  "start_model",
  "stop_model",
  "stop_all",
  "stop_external",
  "logs_tail",
  "presets_list",
  "presets_save",
  "presets_delete",
  "presets_show",
  "favorite_add",
  "favorite_remove",
  "favorite_list",
  "last_params_list",
];

fn supported_methods() -> Vec<&'static str> {
  let mut v = PUBLIC_METHODS.to_vec();
  v.sort();
  v
}

/// Upper bound on `ctx` (token-window) advertised on the IPC. The TUI
/// picker caps at 131_072 but CLI + direct JSON-RPC callers bypass
/// that; this stops a buggy or malicious request from sending
/// `--ctx u32::MAX` straight to llama-server. Shared with config
/// validation via [`crate::config::MAX_CTX_TOKENS`].
use crate::config::MAX_CTX_TOKENS;

/// Output of [`start_model_inner`] — everything the caller needs to
/// observe the launch from the outside. The IPC handler projects
/// this onto the JSON-RPC response; the proxy's auto-start path
/// (Unit 4) keeps the `ManagedModel` handle so it can poll the state
/// machine without going through the registry snapshot.
pub(crate) struct StartedLaunch {
  pub(crate) launch_id: LaunchId,
  pub(crate) model_id: ModelId,
  pub(crate) port: u16,
  pub(crate) model: ManagedModel,
  pub(crate) log_path: PathBuf,
}

/// IPC `start_model` handler — a thin wrapper around
/// [`start_model_inner`]. Keeps the JSON-RPC plumbing (parse params →
/// call inner → JSON-encode response) at the handler boundary so
/// the proxy's auto-start can call the inner helper directly without
/// round-tripping through the dispatcher.
async fn start_model_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: StartParams = parse_params(params)?;
  // IPC clients are user-initiated (TUI Launch, `llamastash start`,
  // bare JSON-RPC). The proxy's auto-start path bypasses this
  // handler and calls `start_model_inner` directly with
  // `LaunchOrigin::AutoStart`.
  let started =
    start_model_inner(ctx, parsed, crate::daemon::supervisor::LaunchOrigin::Manual).await?;
  let pid = started.model.pid().await;
  Ok(json!({
    "launch_id": started.launch_id,
    "model_id": started.model_id,
    "port": started.port,
    "pid": pid,
    "log_path": started.log_path,
  }))
}

/// In-process equivalent of [`start_model_handler`] for callers that
/// already have a parsed [`StartParams`] (the proxy's Unit 4
/// auto-start path). Performs the same composition pipeline —
/// validation → arch resolve → port reservation → layered knob
/// merge → supervisor spawn → registry insert → last_params recorder
/// — so the two call sites share one code path. Returns the live
/// [`StartedLaunch`] handle on success; the [`ErrorObject`] form on
/// any failure stays JSON-RPC compatible so the IPC handler can
/// forward it verbatim.
pub(crate) async fn start_model_inner(
  ctx: &MethodContext,
  parsed: StartParams,
  origin: crate::daemon::supervisor::LaunchOrigin,
) -> Result<StartedLaunch, ErrorObject> {
  // Pure input-validation lives before the daemon's launch-env
  // lookup so a malformed request gives an actionable
  // `InvalidParams` error even on misconfigured daemons.
  if parsed.port.is_some() && parsed.prefer_port.is_some() {
    return Err(ErrorObject::new(
      ErrorCode::InvalidParams,
      "set exactly one of `port` (strict) or `prefer_port` (soft preference)",
    ));
  }
  let env = ctx.launch.as_ref().ok_or_else(|| {
    ErrorObject::new(
      ErrorCode::InternalError,
      "daemon launch environment not configured (binary / port range / log dir missing)",
    )
  })?;

  // Resolve identity + (for GGUF) the architecture. A Lemonade synthetic path
  // (`lemonade://<name>`) has no local file, so derive a backend identity from
  // the registry name instead of reading a header — that is what makes the
  // managed-multiplexer dispatch below select Lemonade rather than crashing on
  // the missing GGUF. Every other path is a local GGUF: one header read yields
  // both the canonical id and the arch.
  let (id, arch, identity): (ModelId, Option<String>, ModelIdentity) =
    match crate::backend::lemonade::registry_name_from_path(&parsed.model_path) {
      Some(name) => {
        let backend_id = crate::backend::identity::BackendModelId {
          backend: crate::backend::lemonade::LEMONADE_BACKEND_ID.to_string(),
          name: name.to_string(),
        };
        // A synthetic ModelId keeps the file-keyed plumbing (log path, running
        // snapshot retention) working; the sentinel header hash marks it as
        // not-a-GGUF. Arch is `None` — lemond owns the recipe, not us.
        let synthetic = ModelId {
          path: parsed.model_path.clone(),
          header_blake3: [0u8; 32],
        };
        (synthetic, None, ModelIdentity::Backend(backend_id))
      }
      None => {
        let (id, arch) = resolve_model_id_and_arch(&parsed.model_path)?;
        let identity: ModelIdentity = id.clone().into();
        (id, arch, identity)
      }
    };

  // Mode resolution: explicit override > catalog hint > default to chat.
  // The CLI surface refuses to default silently when discovery says
  // "Unknown" (cli_args.rs::StartArgs::mode comment), but the daemon
  // is one layer down; a missing override here means the caller has
  // already accepted the default.
  let mode = parsed
    .mode
    .map(LaunchMode::from)
    .unwrap_or(LaunchMode::Chat);

  // Reject pinned port values that would corrupt our internal state
  // or require root: any value below 1024 (which also covers 0, the
  // "OS picks for me" sentinel llama-server would pick a port we
  // never track) needs root and is almost certainly a typo / hostile.
  for p in parsed.port.iter().chain(parsed.prefer_port.iter()) {
    if *p < 1024 {
      return Err(ErrorObject::new(
        ErrorCode::InvalidParams,
        format!("port {p} is not in the allowed range (>= 1024)"),
      ));
    }
  }
  // Validate ctx token-window bound.
  if let Some(c) = parsed.ctx {
    if c > MAX_CTX_TOKENS {
      return Err(ErrorObject::new(
        ErrorCode::InvalidParams,
        format!("ctx {c} exceeds maximum {MAX_CTX_TOKENS}"),
      ));
    }
  }

  // Port allocation — race-safe. `reserve_port` is a CAS across
  // `collect_in_use_ports → allocate → reserve` so two concurrent
  // `start_model` calls cannot both walk away with the same port.
  // We must collect the live in-use list before taking the
  // reservation mutex, since `collect_in_use_ports` itself awaits
  // supervisor read locks.
  let live_in_use = collect_in_use_ports(ctx).await;
  let port = if let Some(preferred) = parsed.prefer_port {
    // Soft preference: try the requested port first; on conflict
    // fall back to the auto-allocator so a returning TUI user
    // doesn't fail launches just because their old port is taken.
    match ctx
      .supervisors
      .reserve_port(Some(preferred), &live_in_use, &env.port_range)
      .await
    {
      Ok(p) => p,
      Err(_) => ctx
        .supervisors
        .reserve_port(None, &live_in_use, &env.port_range)
        .await
        .map_err(|e| {
          ErrorObject::new(
            ErrorCode::InternalError,
            format!("port allocation failed: {e}"),
          )
        })?,
    }
  } else {
    ctx
      .supervisors
      .reserve_port(parsed.port, &live_in_use, &env.port_range)
      .await
      .map_err(|e| {
        ErrorObject::new(
          ErrorCode::InternalError,
          format!("port allocation failed: {e}"),
        )
      })?
  };

  // Compose LaunchParams with the layered resolver. Precedence
  // (highest first): caller-supplied `knobs` → daemon's persisted
  // `last_params` for this model → YAML `arch_defaults[architecture]`
  // → built-in `(arch, backend)` table → llama-server's own default.
  let mut launch_params = LaunchParams::new(parsed.model_path.clone(), mode);
  launch_params.port = Some(port);
  // Per-model backend override (R17): `None` keeps the default `Auto`
  // (identity rule); an explicit choice from `start --backend` / the TUI
  // picker overrides it. Resolved into a backend at the selection seam below.
  launch_params.backend = parsed.backend.unwrap_or_default();
  launch_params.extras = parsed.extras.into_iter().map(OsString::from).collect();
  // Resolve the multimodal projector: an explicit `mmproj_path` wins;
  // otherwise auto-detect a companion next to the model — unless the
  // caller is already managing the projector through `extras`
  // (`--mmproj` to pin a path, `--no-mmproj` to force text-only), in
  // which case auto-detection would only emit a redundant second flag.
  launch_params.mmproj_path = parsed.mmproj_path.clone().or_else(|| {
    if extras_manage_mmproj(&launch_params.extras) {
      None
    } else {
      crate::discovery::scanner::find_mmproj(&parsed.model_path)
    }
  });

  // Merge the caller's top-level `ctx` and `reasoning` into the
  // User-layer typed knobs so they participate in the resolver chain
  // alongside the other typed fields. The wire payload keeps the
  // top-level fields for backward compat with scripted clients —
  // they're projected onto the typed knob slots here, with explicit
  // `knobs.{ctx,reasoning}` overrides winning if the caller set both.
  let mut user_knobs = parsed.knobs.clone();
  if user_knobs.ctx.is_none() {
    user_knobs.ctx = parsed.ctx.map(KnobValue::Set);
  }
  if user_knobs.reasoning.is_none() {
    user_knobs.reasoning = parsed.reasoning.map(KnobValue::Set);
  }

  // Pull the model's last_params from persisted state so a returning
  // user inherits the knobs they last shipped (R20 precedence).
  let last_params_knobs = {
    let snap = ctx.state.snapshot().await;
    snap
      .last_params_map()
      .get(&identity)
      .map(|p| p.knobs.clone())
      .unwrap_or_default()
  };
  let empty_yaml = crate::config::TypedKnobs::default();
  let yaml_knobs = arch
    .as_deref()
    .and_then(|a| env.arch_defaults.get(a))
    .unwrap_or(&empty_yaml);
  let backend = current_backend_flavor(ctx).await;
  let builtin_knobs = match arch.as_deref() {
    Some(a) => crate::launch::defaults_table::lookup(a, backend),
    None => crate::launch::defaults_table::lookup("", backend),
  };
  // yaml + built-in share the `ArchDefault` chip — yaml wins per
  // field via precedence order.
  let mut resolved = crate::launch::params::resolve_layered(&[
    (crate::launch::params::LayerLabel::User, &user_knobs),
    (
      crate::launch::params::LayerLabel::LastUsed,
      &last_params_knobs,
    ),
    (crate::launch::params::LayerLabel::ArchDefault, yaml_knobs),
    (
      crate::launch::params::LayerLabel::ArchDefault,
      &builtin_knobs,
    ),
  ]);
  // Seed knobs no layer filled per the default launch mode (R1): under
  // `Auto` a layer-less knob delegates to `--fit`. Argv-neutral in this
  // unit (an Auto knob emits nothing, exactly like the unset slot it
  // replaces) — the fit-flag emission lands in U6 and the picker
  // rendering in U10. The mode is `Config.default_launch_mode`
  // (+ `LLAMASTASH_DEFAULT_LAUNCH_MODE`), threaded through `LaunchEnv`.
  crate::launch::params::seed_layerless(&mut resolved, env.default_launch_mode);
  // Project resolved ctx/reasoning back onto the top-level
  // `LaunchParams` fields — `compose` emits them inline (ctx as
  // `-c <N>`, reasoning as the `--jinja --reasoning-format deepseek`
  // bundle).
  // An `Auto` ctx/reasoning collapses to "no inline flag" here
  // (`set_value()` → `None`): `compose` emits nothing and `--fit`
  // governs ctx, the chat template governs reasoning.
  launch_params.ctx = resolved.knobs.ctx.set_value().copied();
  launch_params.reasoning = resolved
    .knobs
    .reasoning
    .set_value()
    .copied()
    .unwrap_or(false);
  launch_params.knobs = resolved.knobs;
  // Close the `knobs.ctx` bypass of `MAX_CTX_TOKENS` (the early check
  // only saw the top-level `parsed.ctx`): validate the *resolved* ctx,
  // which folds in both `parsed.ctx` and a typed `knobs.ctx` set via the
  // editor or last-params.
  if let Some(c) = launch_params.ctx {
    if c > MAX_CTX_TOKENS {
      return Err(ErrorObject::new(
        ErrorCode::InvalidParams,
        format!("ctx {c} exceeds maximum {MAX_CTX_TOKENS}"),
      ));
    }
  }
  // Leave `device` exactly as the resolver chain set it. When no layer
  // selected one it stays `None`, so `compose()` emits no `--device`
  // and `llama-server` keeps its default (auto-select / split across
  // every visible GPU) — the documented backwards-compatible behavior.

  // Context sizing is delegated to llama-server's `--fit`: when `ctx`
  // is unset (Auto / Inherited), `compose` emits `--fit-ctx <floor>` so
  // fit sizes the window for the available memory but never collapses
  // below the floor. llamastash keeps budget *authority* via U8
  // admission (the sysfs-backed pool reading), not by computing ctx
  // here — the old `ctx_fit` GPU path is retired. A pinned `ctx`
  // suppresses the floor (fit honors the pin).
  launch_params.fit_ctx_floor = Some(env.fit_ctx_floor);

  // Reject loopback-breaking / auth-bypass extras flags before
  // spawn. `compose` strips defensively too, but failing fast here
  // gives callers a clear error instead of a silently-different argv.
  // Release the reservation first so a retry can re-use the port —
  // otherwise a client that repeatedly submits a banned flag would
  // permanently exhaust the port pool.
  let banned = crate::launch::params::forbidden_in_extras(&launch_params.extras);
  if !banned.is_empty() {
    ctx.supervisors.release_reserved_port(port).await;
    return Err(ErrorObject::new(
      ErrorCode::InvalidParams,
      format!(
        "extras flags refused (loopback / auth contract): {}",
        banned.join(", ")
      ),
    ));
  }

  // Per-launch log file under cache_dir/logs/<short-id>-<ts>.log.
  let log_path = build_log_path(&env.log_dir, &id);

  // Scale the probe budget by total weight bytes so a slow load
  // (large multipart GGUF, HIP/ROCm upload, cold disk) doesn't trip
  // the default 120 s timeout. The catalog row carries the
  // multipart-aware total via `discovery::shard_sizes`. Fall back to
  // the path's on-disk total when the model isn't in the catalog
  // (direct-path launches that bypass scan).
  let total_weight_bytes = launch_total_bytes(ctx, &launch_params.model_path).await;
  let scaled_probe = env.probe.scale_for_model(total_weight_bytes);

  // Pick the binary that owns the chosen `--device` selector. The
  // selector (`Vulkan0`, `CUDA0`, …) came from a specific binary's
  // `--list-devices`, so we must spawn *that* binary or the selector
  // is invalid. Unset / empty device falls back to the default binary
  // with no `--device`.
  let selector = launch_params
    .knobs
    .device
    .set_value()
    .map(String::as_str)
    .filter(|s| !s.is_empty())
    .map(str::to_string);
  let launch_binary = match selector {
    Some(sel) => {
      let owning_binary = {
        let catalog = env.device_catalog.read().await;
        catalog
          .iter()
          .find(|d| d.selector == sel)
          .map(|d| d.binary.clone())
      };
      owning_binary.unwrap_or_else(|| {
        // Stale persisted selector or the catalog probe failed. Drop
        // the selector so `compose()` doesn't emit an invalid
        // `--device` the default binary would reject, and spawn the
        // default binary with auto-select.
        log::warn!(
          "device selector {sel:?} not in launch catalog; dropping it and spawning default binary {}",
          env.binary.display()
        );
        launch_params.knobs.device = None;
        env.binary.clone()
      })
    }
    None => env.binary.clone(),
  };

  // Translate the resolved knob IR into a launch plan via the backend.
  // Selection honors the per-model override then the R13 identity rule
  // (`Auto` → GGUF binds llama.cpp). The orchestrator owns the branch on
  // plan shape: the process-per-model arm feeds the supervisor; the
  // managed-multiplexer arm ensures the shared umbrella + delegates.
  let inference_backend = crate::backend::resolve_backend(&identity, launch_params.backend);

  // A managed-multiplexer backend (Lemonade) supervises its *own* umbrella
  // executable on its *own* loopback port — not the per-model `llama-server`
  // binary or the reserved launch-pool port. Resolve the `lemond` binary
  // (config path → PATH) and use the configured umbrella port so the umbrella
  // discovery probes and the routing target all agree. The reserved pool port
  // is released by `start_delegated_lemonade` once the umbrella owns its port.
  let (plan_binary, plan_port) =
    if inference_backend.lifecycle() == crate::backend::Lifecycle::ManagedMultiplexer {
      match crate::backend::lemonade::resolve_lemond_binary(&ctx.lemonade) {
        Some(bin) => (bin, ctx.lemonade.port),
        None => {
          ctx.supervisors.release_reserved_port(port).await;
          return Err(ErrorObject::new(
            ErrorCode::InvalidParams,
            "lemonade backend selected but no `lemond` binary found; set `lemonade.binary` \
             or put `lemond` on PATH (see docs/lemonade-setup.md)"
              .to_string(),
          ));
        }
      }
    } else {
      (launch_binary, port)
    };

  let launch_spec =
    match inference_backend.prepare_launch(&launch_params, plan_port, plan_binary, scaled_probe) {
      LaunchPlan::SpawnProcess(spec) => spec,
      LaunchPlan::DelegateToManager(spec) => {
        // Managed-multiplexer (Lemonade): ensure the shared umbrella is up on
        // `plan_port` and delegate the model to it, rather than spawning a
        // per-model child. The umbrella binds its own configured port, not the
        // launch-pool reservation, so release `port` first (holding it would
        // leak a pool slot). Routing of a Lemonade model's requests through the
        // proxy is handled separately (catalog-source-based; see
        // `proxy::route`).
        ctx.supervisors.release_reserved_port(port).await;
        return start_delegated_lemonade(
          ctx,
          spec,
          plan_port,
          id,
          identity,
          log_path,
          launch_params,
        )
        .await;
      }
    };

  // Pre-spawn admission (R4): project this launch's demand floor and
  // refuse *before* spawn if it won't fit the sampled budget minus the
  // bytes other in-flight launches already reserved. This is the safety
  // net `--fit` can't provide on UMA (its free reading conflates GTT
  // with system RAM). Keyed by the reserved `port` (unique per in-flight
  // launch); released when the child settles or on any failure below.
  // Best-effort: skipped entirely when there is no host-metrics sample
  // yet (the `port` reservation still gates the pool). Only the
  // process-spawn path reaches here — Lemonade's umbrella returned
  // above.
  let mut admitted = false;
  if identity.as_gguf().is_some() {
    if let Some(host_slot) = ctx.host_metrics.as_ref() {
      let snapshot = host_slot.read().await.clone();
      if crate::launch::admission::is_sampled(&snapshot) {
        let effective_ctx = launch_params.ctx.unwrap_or(env.fit_ctx_floor);
        let free = crate::launch::admission::effective_free_bytes(&snapshot);
        let gpu_backend = snapshot.gpu_backend.clone();
        let model_path = launch_params.model_path.clone();
        let knobs = launch_params.knobs.clone();
        let arch_owned = arch.clone();
        let demand = tokio::task::spawn_blocking(move || {
          let header = read_gguf_header(&model_path, HeaderReadOptions::default())
            .ok()?
            .header;
          Some(crate::launch::admission::project_demand(
            &header,
            arch_owned.as_deref(),
            &knobs,
            effective_ctx,
            &gpu_backend,
          ))
        })
        .await
        .ok()
        .flatten();
        if let Some(demand) = demand {
          if let Err(refusal) = ctx.admission.try_admit(u64::from(port), demand, free) {
            ctx.supervisors.release_reserved_port(port).await;
            return Err(ErrorObject::with_data(
              ErrorCode::InternalError,
              format_admission_refusal(&refusal),
              serde_json::json!({ "cause": "launch_refused" }),
            ));
          }
          admitted = true;
        }
      }
    }
  }

  let spawn_result = supervisor_spawn(ManagedSpawn {
    id: id.clone(),
    params: launch_params.clone(),
    port,
    mode,
    log_path: log_path.clone(),
    plan: launch_spec,
    origin,
  })
  .await;
  let model = match spawn_result {
    Ok(m) => m,
    Err(e) => {
      // Free the reserved port + admission hold so a retry can re-use them.
      ctx.supervisors.release_reserved_port(port).await;
      if admitted {
        ctx.admission.release(u64::from(port));
      }
      return Err(ErrorObject::new(
        ErrorCode::InternalError,
        format!("supervisor spawn: {e}"),
      ));
    }
  };

  let launch_id = ctx.supervisors.next_id();
  ctx
    .supervisors
    .insert(launch_id.clone(), model.clone())
    .await;
  // Live supervisor now owns the port; drop the in-flight reservation.
  ctx.supervisors.release_reserved_port(port).await;

  // Persist running snapshot. Retain by `(id, port)` so the same
  // GGUF launched twice against different ports persists both
  // snapshots — the orphan sweep can then re-adopt either one on
  // daemon restart instead of silently dropping the older.
  let pid = model.pid().await.unwrap_or(0) as i32;
  let started_at = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or_default();
  ctx
    .state
    .mutate(|s| {
      s.running.retain(|r| !(r.id == identity && r.port == port));
      s.running.push(RunningSnapshot {
        id: identity.clone(),
        pid,
        port,
        started_at,
        params: launch_params.clone(),
        actuals: Default::default(),
      });
    })
    .await;

  // Background task: when the supervisor reaches Ready, stamp
  // last_params (per the plan — only updated on a *successful*
  // Loading → Ready transition). We poll because ManagedModel
  // doesn't expose a transition channel yet.
  //
  // Persist the *user-supplied* knob deltas, not the full resolved set
  // — so source chips in the picker stay meaningful (a knob the user
  // never touched keeps re-resolving from yaml / built-in / model
  // default instead of being frozen as `(last used)`). "Remembered
  // values win" depends on this: only what the user actually set
  // (including an explicit `Auto` sentinel) is remembered, so the
  // resolver re-derives the rest next launch. The resolved top-level
  // `ctx`/`reasoning` and the force-copied `device` are dropped too —
  // they were resolver output, not user intent, and re-pinning them
  // would freeze a value the user never chose.
  let mut persist_params = launch_params.clone();
  persist_params.knobs = user_knobs;
  persist_params.ctx = None;
  persist_params.reasoning = false;
  spawn_last_params_recorder(
    ctx.state.clone(),
    model.clone(),
    identity.clone(),
    persist_params,
    // Slow HIP/Metal loads routinely exceed the old fixed 180 s wall
    // clock; key the recorder's deadline off the same size-scaled probe
    // budget the supervisor uses (base +2 h cap) so a slow load still
    // reaches Ready *and* gets its params recorded — otherwise the next
    // launch finds no remembered value and wrongly seeds Auto.
    scaled_probe.timeout,
    ctx.shutdown.clone(),
  );

  // Settle the admission reservation when the child leaves Loading
  // (Ready: real allocation is now visible to the sampler; Error /
  // Stopped: the slot is freed). Keyed by `port`; idempotent.
  if admitted {
    spawn_admission_settle(
      ctx.admission.clone(),
      model.clone(),
      port,
      scaled_probe.timeout,
      ctx.shutdown.clone(),
    );
  }

  Ok(StartedLaunch {
    launch_id,
    model_id: id,
    port,
    model,
    log_path,
  })
}

/// Start a model on a managed-multiplexer backend (Lemonade): ensure the
/// shared `lemond` umbrella is supervised + ready, then preload the model
/// through its API. Returns a [`StartedLaunch`] anchored on the umbrella
/// (the supervised process), so status + stop see one umbrella for all
/// Lemonade models. Routing of inference is handled by the proxy
/// (catalog-source-based; see [`crate::proxy::route`]).
async fn start_delegated_lemonade(
  ctx: &MethodContext,
  spec: crate::backend::ManagerLaunchSpec,
  umbrella_port: u16,
  id: ModelId,
  identity: ModelIdentity,
  log_path: PathBuf,
  params: LaunchParams,
) -> Result<StartedLaunch, ErrorObject> {
  use crate::backend::lemonade::{ensure_umbrella, umbrella_launch_id, LemonadeClient};

  let umbrella = match ensure_umbrella(
    &ctx.supervisors,
    umbrella_port,
    spec.umbrella,
    log_path.clone(),
  )
  .await
  {
    Ok(m) => m,
    Err(e) => {
      return Err(ErrorObject::new(
        ErrorCode::InternalError,
        format!("lemonade umbrella failed to start: {e}"),
      ));
    }
  };
  // An already-running umbrella keeps its own port; trust the handle over the
  // requested `umbrella_port` so a reused umbrella routes to the right place.
  let serving_port = umbrella.port();

  // Preload the model so an explicit launch makes it resident (chat would
  // autoload too), forwarding the launch params lemond honors: `ctx_size`
  // plus the free-form extras as the recipe-scoped `*_args` string.
  //
  // The load runs as a background task: a cold load can take lemond's
  // full 120 s budget, which is far past the CLI's IPC reply timeout —
  // awaiting it here meant the client hung up and hyper cancelled this
  // handler mid-preload, silently dropping the launch. The task records
  // its outcome in the registry's delegated-state map (`Loading` →
  // `Ready` / `Error{cause}`), which is what `status` reports for the
  // row — so a model lemond can't load shows `error` with lemond's
  // message instead of a phantom `ready`.
  ctx
    .supervisors
    .set_delegated_state(&spec.model.name, ManagedState::Loading)
    .await;
  {
    let registry = ctx.supervisors.clone();
    let name = spec.model.name.clone();
    let params = params.clone();
    tokio::spawn(async move {
      let outcome = match LemonadeClient::new(serving_port) {
        Ok(client) => {
          let opts = lemonade_load_options(&client, &name, &params).await;
          client.load_with(&name, &opts).await
        }
        Err(e) => Err(e),
      };
      match outcome {
        Ok(()) => {
          registry
            .set_delegated_state(&name, ManagedState::Ready)
            .await;
        }
        Err(e) => {
          log::warn!("lemonade: preload of `{name}` failed: {e}");
          registry
            .set_delegated_state(
              &name,
              ManagedState::Error {
                cause: e.to_string(),
              },
            )
            .await;
        }
      }
    });
  }

  // Persist a running snapshot at the umbrella port for status visibility.
  let pid = umbrella.pid().await.unwrap_or(0) as i32;
  let started_at = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or_default();
  ctx
    .state
    .mutate(|s| {
      s.running
        .retain(|r| !(r.id == identity && r.port == serving_port));
      s.running.push(RunningSnapshot {
        id: identity.clone(),
        pid,
        port: serving_port,
        started_at,
        params: params.clone(),
        actuals: Default::default(),
      });
    })
    .await;

  Ok(StartedLaunch {
    launch_id: umbrella_launch_id(),
    model_id: id,
    port: serving_port,
    model: umbrella,
    log_path,
  })
}

/// Project the launch params onto lemond's load-option surface: `ctx`
/// (when the user set one) becomes `ctx_size`; non-empty extras become
/// the recipe-scoped args string (`llamacpp_args` / `whispercpp_args` /
/// `flm_args` — lemond names the field after the model's recipe, read
/// from the umbrella's own model list). Extras are dropped with a
/// warning when the recipe can't be resolved — guessing the field would
/// silently feed flags to the wrong engine.
async fn lemonade_load_options(
  client: &crate::backend::lemonade::LemonadeClient,
  name: &str,
  params: &LaunchParams,
) -> crate::backend::lemonade::LoadOptions {
  let recipe_args = if params.extras.is_empty() {
    None
  } else {
    let joined = params
      .extras
      .iter()
      .map(|s| s.to_string_lossy())
      .collect::<Vec<_>>()
      .join(" ");
    let recipe = client
      .list_model_entries()
      .await
      .ok()
      .and_then(|entries| entries.into_iter().find(|e| e.id == name))
      .and_then(|e| e.recipe);
    match recipe {
      Some(recipe) => Some((format!("{recipe}_args"), joined)),
      None => {
        log::warn!(
          "lemonade: dropping extras for `{name}` — could not resolve its recipe \
           from the umbrella's model list"
        );
        None
      }
    }
  };
  crate::backend::lemonade::LoadOptions {
    ctx_size: params.ctx,
    recipe_args,
  }
}

/// Human-readable admission refusal: the effective free (post-headroom),
/// what other launches hold, this launch's projected demand, and the
/// remediation menu — so the number is self-explaining and actionable.
fn format_admission_refusal(refusal: &crate::launch::admission::Refusal) -> String {
  let gib = |b: u64| format!("{:.1} GiB", b as f64 / (1024.0 * 1024.0 * 1024.0));
  format!(
    "launch refused: needs {} but only {} is free (effective {} after headroom, minus {} reserved by in-flight launches). \
     Stop a resident model, pin a smaller --ctx, lower fit_ctx_floor, or retry once a model frees memory.",
    gib(refusal.demand_bytes),
    gib(refusal.available_bytes()),
    gib(refusal.effective_free_bytes),
    gib(refusal.reserved_bytes),
  )
}

/// Poll the child until it leaves Loading (Ready / Error / Stopped) and
/// drop its admission reservation. Mirrors the recorder's poll shape;
/// bounded by the scaled probe budget and the shutdown token so a child
/// that never settles can't leak the reservation forever.
fn spawn_admission_settle(
  ledger: Arc<crate::launch::admission::Ledger>,
  model: ManagedModel,
  port: u16,
  probe_budget: Duration,
  shutdown: ShutdownToken,
) {
  tokio::spawn(async move {
    let deadline = Instant::now() + probe_budget;
    loop {
      match model.state().await {
        ManagedState::Ready | ManagedState::Error { .. } | ManagedState::Stopped => {
          ledger.release(u64::from(port));
          return;
        }
        _ => {}
      }
      if Instant::now() > deadline {
        ledger.release(u64::from(port));
        return;
      }
      tokio::select! {
        _ = shutdown.wait_until_triggered() => {
          ledger.release(u64::from(port));
          return;
        }
        _ = tokio::time::sleep(Duration::from_millis(200)) => {}
      }
    }
  });
}

fn spawn_last_params_recorder(
  state: PersistedState,
  model: ManagedModel,
  id: ModelIdentity,
  params: LaunchParams,
  probe_budget: Duration,
  shutdown: ShutdownToken,
) {
  tokio::spawn(async move {
    // Wait out the same size-scaled probe budget the supervisor uses
    // (base 120 s + up to 2 h for very large weights) so a slow load
    // still gets its params recorded on the Loading → Ready transition.
    // The poll also observes the daemon's shutdown token so SIGTERM
    // during a pending Loading state doesn't block clean process exit.
    let deadline = Instant::now() + probe_budget;
    loop {
      match model.state().await {
        ManagedState::Ready => {
          state
            .mutate(|s| s.upsert_last_params(id.clone(), params.clone()))
            .await;
          // Post-launch actuals (R6): read what `--fit` actually chose
          // from the child's `/props` and stamp it on the running
          // snapshot so `status` / the TUI Running view / `show` can
          // render the resolved context. Best-effort — an empty result
          // (no `/props`, transport error) leaves the row "unavailable".
          if let Some(port) = params.port {
            let actuals = crate::daemon::actuals::fetch(port, Duration::from_secs(5)).await;
            if !actuals.is_empty() {
              let id = id.clone();
              state
                .mutate(move |s| {
                  if let Some(snap) = s.running.iter_mut().find(|r| r.id == id && r.port == port) {
                    snap.actuals = actuals;
                  }
                })
                .await;
            }
          }
          return;
        }
        ManagedState::Error { .. } | ManagedState::Stopped => return,
        _ => {}
      }
      if Instant::now() > deadline {
        return;
      }
      tokio::select! {
        _ = shutdown.wait_until_triggered() => return,
        _ = tokio::time::sleep(Duration::from_millis(200)) => {}
      }
    }
  });
}

async fn collect_in_use_ports(ctx: &MethodContext) -> Vec<u16> {
  let mut ports: Vec<u16> = ctx
    .supervisors
    .snapshot()
    .await
    .into_iter()
    .map(|(_, m)| m.port())
    .collect();
  // Also avoid colliding with `llama-server` processes that this
  // daemon didn't spawn directly but were launched by *some*
  // llamastash instance — typically a previous run of this daemon
  // or a sibling UAT/test daemon whose state.json the orphan sweep
  // didn't see. The `LLAMASTASH_LAUNCHED=1` env marker (stamped by
  // `supervisor::spawn`) is what makes these recognisable; the port
  // gets parsed out of the orphan's argv in `orphans::sweep`.
  //
  // The bind probe in `ports::try_bind_probe` already rejects an
  // externally-held port at reservation time, so this list is a
  // hint to the allocator rather than a correctness gate — it just
  // lets us skip straight past known-busy slots instead of probing
  // them one by one on every launch.
  let externals = ctx.external.read().await;
  for ext in externals.iter() {
    if ext.launched_by_llamastash {
      if let Some(p) = ext.port {
        if !ports.contains(&p) {
          ports.push(p);
        }
      }
    }
  }
  ports
}

fn resolve_model_id(path: &std::path::Path) -> Result<ModelId, ErrorObject> {
  let (id, _) = resolve_model_id_and_arch(path)?;
  Ok(id)
}

/// One-pass GGUF header read that returns both the canonical model id
/// and the architecture string. `start_model_handler` calls this so
/// the layered-knob resolver lookup doesn't have to re-read the
/// header to discover the arch. Arch is best-effort: a `None` here
/// just means the `defaults_table` lookup falls back to the `*` row.
fn resolve_model_id_and_arch(
  path: &std::path::Path,
) -> Result<(ModelId, Option<String>), ErrorObject> {
  let header = read_gguf_header(path, HeaderReadOptions::default()).map_err(|e| {
    ErrorObject::new(
      ErrorCode::InvalidParams,
      format!("could not read GGUF header at {}: {e}", path.display()),
    )
  })?;
  let id = compute_model_id(path, &header.raw);
  let arch = crate::gguf::metadata::summarise(&header.header).arch;
  Ok((id, arch))
}

/// Does the caller's `extras` tail already manage the multimodal
/// projector? `--mmproj <path>` pins a projector and `--no-mmproj`
/// force-disables it; in either case the daemon must not auto-detect
/// one too. Matches the flag head in both space form (`--mmproj`) and
/// equals form (`--mmproj=/path`), case-insensitively. `--no-mmproj-offload`
/// (offload tuning, projector still on) is left to auto-detect, so the
/// match is exact rather than a prefix test.
fn extras_manage_mmproj(extras: &[OsString]) -> bool {
  extras.iter().any(|e| {
    let lossy = e.to_string_lossy();
    let head = lossy.split('=').next().unwrap_or(&lossy);
    head.eq_ignore_ascii_case("--mmproj") || head.eq_ignore_ascii_case("--no-mmproj")
  })
}

/// Total on-disk weight bytes for the model the launch handler is
/// about to spawn. Prefers the catalog row (which already includes
/// split-shard aggregation via `discovery::shard_sizes`); falls back
/// to `shard_sizes::on_disk_total` on the bare path for direct
/// launches that bypass scan. `0` when neither path is reachable —
/// the probe scaler treats that as "no signal, keep the default".
async fn launch_total_bytes(ctx: &MethodContext, model_path: &std::path::Path) -> u64 {
  let snap = ctx.catalog.snapshot().await;
  if let Some(row) = snap.iter().find(|m| m.path == model_path) {
    if let Some(b) = row.metadata.as_ref().and_then(|md| md.weights_bytes) {
      return b;
    }
    return crate::discovery::shard_sizes::on_disk_total(&row.path, &row.split_siblings);
  }
  crate::discovery::shard_sizes::on_disk_total(model_path, &[])
}

/// Live GPU-backend flavor — keys the built-in defaults table.
/// Reads the host-metrics sampler when available; falls back to
/// `Unsampled` (treated identically to `Unknown` by the table) when
/// the daemon has no sampler attached (catalog-only tests).
async fn current_backend_flavor(ctx: &MethodContext) -> crate::daemon::host_metrics::GpuFlavor {
  if let Some(slot) = &ctx.host_metrics {
    let snap = slot.read().await;
    return snap.flavor();
  }
  crate::daemon::host_metrics::GpuFlavor::Unsampled
}

fn build_log_path(log_dir: &std::path::Path, id: &ModelId) -> PathBuf {
  let stem = id
    .path
    .file_stem()
    .and_then(|s| s.to_str())
    .unwrap_or("model");
  let ts = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or_default();
  let short = id.short_fingerprint();
  log_dir.join(format!("{stem}-{short}-{ts}.log"))
}

#[derive(Deserialize)]
struct PresetsListParams {
  model_path: PathBuf,
}

async fn presets_list_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: PresetsListParams = parse_params(params)?;
  let id = resolve_model_id(&parsed.model_path)?;
  let identity = ModelIdentity::Gguf(id.clone());
  let snapshot = ctx.state.snapshot().await;
  let presets = snapshot
    .presets_map()
    .get(&identity)
    .cloned()
    .unwrap_or_default();
  Ok(json!({
    "model_id": id,
    "presets": presets.iter().map(preset_row).collect::<Vec<_>>(),
  }))
}

#[derive(Deserialize)]
struct PresetsSaveParams {
  model_path: PathBuf,
  name: String,
  #[serde(default)]
  ctx: Option<u32>,
  #[serde(default)]
  port: Option<u16>,
  #[serde(default)]
  reasoning: Option<bool>,
  #[serde(default)]
  mode: Option<LaunchModeWire>,
  #[serde(default)]
  knobs: crate::config::TypedKnobs,
  #[serde(default)]
  extras: Vec<String>,
}

async fn presets_save_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: PresetsSaveParams = parse_params(params)?;
  if parsed.name.trim().is_empty() {
    return Err(ErrorObject::new(
      ErrorCode::InvalidParams,
      "preset name must not be empty",
    ));
  }
  let id = resolve_model_id(&parsed.model_path)?;
  let identity = ModelIdentity::Gguf(id.clone());
  let mut params_value = LaunchParams::new(
    parsed.model_path.clone(),
    parsed
      .mode
      .map(LaunchMode::from)
      .unwrap_or(LaunchMode::Chat),
  );
  params_value.ctx = parsed.ctx;
  params_value.port = parsed.port;
  params_value.reasoning = parsed.reasoning.unwrap_or(false);
  params_value.knobs = parsed.knobs;
  params_value.extras = parsed.extras.into_iter().map(OsString::from).collect();
  let preset = NamedPreset {
    name: parsed.name.clone(),
    params: params_value.clone(),
  };

  let prev = ctx
    .state
    .mutate(|s| {
      let mut presets = s.presets_map().get(&identity).cloned().unwrap_or_default();
      let prev = presets.upsert(preset.clone());
      s.upsert_presets(identity.clone(), presets);
      prev
    })
    .await;

  Ok(json!({
    "model_id": id,
    "saved": preset_row(&preset),
    "replaced": prev.as_ref().map(preset_row),
  }))
}

#[derive(Deserialize)]
struct PresetsDeleteParams {
  model_path: PathBuf,
  name: String,
}

async fn presets_delete_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: PresetsDeleteParams = parse_params(params)?;
  let id = resolve_model_id(&parsed.model_path)?;
  let identity = ModelIdentity::Gguf(id.clone());
  let removed = ctx
    .state
    .mutate(|s| {
      let mut presets = s.presets_map().get(&identity).cloned().unwrap_or_default();
      let removed = presets.remove(&parsed.name);
      s.upsert_presets(identity.clone(), presets);
      removed
    })
    .await;
  Ok(json!({
    "model_id": id,
    "removed": removed.as_ref().map(preset_row),
  }))
}

#[derive(Deserialize)]
struct PresetsShowParams {
  model_path: PathBuf,
  name: String,
}

async fn presets_show_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: PresetsShowParams = parse_params(params)?;
  let id = resolve_model_id(&parsed.model_path)?;
  let identity = ModelIdentity::Gguf(id.clone());
  let snapshot = ctx.state.snapshot().await;
  let preset = snapshot
    .presets_map()
    .get(&identity)
    .and_then(|p| p.get(&parsed.name).cloned());
  Ok(json!({
    "model_id": id,
    "preset": preset.as_ref().map(preset_row),
  }))
}

fn preset_row(p: &NamedPreset) -> Value {
  json!({
    "name": p.name,
    "params": launch_params_row(&p.params),
  })
}

fn launch_params_row(p: &LaunchParams) -> Value {
  json!({
    "model_path": p.model_path,
    "mode": p.mode.label(),
    "ctx": p.ctx,
    "port": p.port,
    "reasoning": p.reasoning,
    "knobs": &p.knobs,
    "extras": p.extras.iter().map(|s| s.to_string_lossy().into_owned()).collect::<Vec<_>>(),
  })
}

#[derive(Deserialize)]
struct FavoriteParams {
  model_path: PathBuf,
}

async fn favorite_add_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: FavoriteParams = parse_params(params)?;
  let id = resolve_model_id(&parsed.model_path)?;
  let identity = ModelIdentity::Gguf(id.clone());
  let added = ctx
    .state
    .mutate(|s| s.favorites.add(identity.clone()))
    .await;
  Ok(json!({
    "model_id": id,
    "added": added,
  }))
}

async fn favorite_remove_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: FavoriteParams = parse_params(params)?;
  let id = resolve_model_id(&parsed.model_path)?;
  let identity = ModelIdentity::Gguf(id.clone());
  let removed = ctx.state.mutate(|s| s.favorites.remove(&identity)).await;
  Ok(json!({
    "model_id": id,
    "removed": removed,
  }))
}

async fn favorite_list_handler(ctx: &MethodContext) -> Result<Value, ErrorObject> {
  let snapshot = ctx.state.snapshot().await;
  let entries: Vec<&FavoriteEntry> = snapshot.favorites.iter().collect();
  let body: Vec<Value> = entries.iter().map(|e| json!({"id": &e.id})).collect();
  Ok(json!({"favorites": body}))
}

/// Snapshot every persisted `last_params` entry. Used by the TUI to
/// pre-populate the launch picker with the most recent successful
/// launch params for the focused model (plan: "the picker is
/// pre-populated with last-params and named-preset values"). Keyed
/// by `model_path` so the TUI can look up without re-resolving
/// `ModelId`.
async fn last_params_list_handler(ctx: &MethodContext) -> Result<Value, ErrorObject> {
  let snapshot = ctx.state.snapshot().await;
  let rows: Vec<Value> = snapshot
    .last_params
    .iter()
    .map(|entry| {
      json!({
        "id": &entry.id,
        "model_path": entry.id.as_gguf().map(|g| &g.path),
        "params": launch_params_row(&entry.params),
      })
    })
    .collect();
  Ok(json!({ "last_params": rows }))
}

fn parse_params<T: serde::de::DeserializeOwned>(params: Option<Value>) -> Result<T, ErrorObject> {
  let raw = params.unwrap_or(Value::Null);
  serde_json::from_value(raw)
    .map_err(|e| ErrorObject::new(ErrorCode::InvalidParams, format!("params parse error: {e}")))
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  fn ctx() -> MethodContext {
    MethodContext::new(ShutdownToken::new())
  }

  #[test]
  fn extras_manage_mmproj_detects_explicit_projector_flags() {
    let pin = vec![OsString::from("--mmproj"), OsString::from("/m/p.gguf")];
    assert!(extras_manage_mmproj(&pin), "space-form --mmproj");
    let pin_eq = vec![OsString::from("--MMPROJ=/m/p.gguf")];
    assert!(
      extras_manage_mmproj(&pin_eq),
      "equals-form, case-insensitive"
    );
    let disable = vec![OsString::from("--no-mmproj")];
    assert!(extras_manage_mmproj(&disable), "--no-mmproj force-disable");
    // Offload tuning leaves the projector on → auto-detect still runs.
    let offload = vec![OsString::from("--no-mmproj-offload")];
    assert!(
      !extras_manage_mmproj(&offload),
      "--no-mmproj-offload is not projector management"
    );
    let unrelated = vec![OsString::from("--threads"), OsString::from("8")];
    assert!(!extras_manage_mmproj(&unrelated));
  }

  #[tokio::test]
  async fn ping_returns_pong() {
    let req = Request::new(1, "ping", None);
    let resp = dispatch_request(&ctx(), req).await;
    assert_eq!(resp.result, Some(json!("pong")));
    assert!(resp.error.is_none());
  }

  #[tokio::test]
  async fn version_reports_package_metadata() {
    let resp = dispatch_request(&ctx(), Request::new(1, "version", None)).await;
    let body = resp.result.expect("version returns result");
    assert_eq!(body["name"], json!(env!("CARGO_PKG_NAME")));
    assert_eq!(body["version"], json!(env!("CARGO_PKG_VERSION")));
    assert!(body["pid"].is_number());
    assert!(body["uptime_seconds"].is_number());
    assert_eq!(body["connections"], json!(0));
  }

  #[tokio::test]
  async fn capabilities_reports_sorted_public_method_surface() {
    let resp = dispatch_request(&ctx(), Request::new(1, "capabilities", None)).await;
    let body = resp.result.expect("capabilities returns result");
    let methods = body["methods"].as_array().expect("methods array");
    let methods: Vec<&str> = methods
      .iter()
      .map(|v| v.as_str().expect("method names are strings"))
      .collect();

    let mut expected = PUBLIC_METHODS.to_vec();
    expected.sort();
    assert_eq!(methods, expected);
  }

  #[tokio::test]
  async fn shutdown_triggers_token() {
    let c = ctx();
    let token = c.shutdown.clone();
    let resp = dispatch_request(&c, Request::new(1, "shutdown", None)).await;
    assert!(resp.error.is_none());
    assert!(token.is_triggered(), "shutdown method must trip the token");
  }

  #[tokio::test]
  async fn unknown_method_returns_method_not_found() {
    let resp = dispatch_request(&ctx(), Request::new(1, "no-such", None)).await;
    let err = resp.error.expect("unknown method must error");
    assert_eq!(err.code, ErrorCode::MethodNotFound.as_i32());
    assert!(
      err.message.contains("no-such"),
      "error message should name the missing method, got: {}",
      err.message
    );
  }

  #[tokio::test]
  async fn list_models_returns_catalog_snapshot() {
    use std::path::PathBuf;

    use crate::discovery::{DiscoveredModel, ModelSource};
    use crate::gguf::metadata::{ModeHint, ModelMetadata, Quant};

    let catalog = ModelCatalog::new();
    catalog
      .upsert(DiscoveredModel {
        path: PathBuf::from("/m/seed.gguf"),
        parent: PathBuf::from("/m"),
        source: ModelSource::HuggingFace,
        metadata: Some(ModelMetadata {
          arch: Some("llama".to_string()),
          total_parameters: Some(7_000_000_000),
          parameter_label: Some("7B".to_string()),
          quant: Quant::Q4_K,
          native_ctx: Some(8192),
          chat_template: None,
          tokenizer_kind: Some("llama".to_string()),
          reasoning_hint: false,
          mode_hint: ModeHint::Chat,
          weights_bytes: Some(4_000_000_000),
        }),
        parse_error: None,
        split_siblings: Vec::new(),
        display_label: None,
        multimodal: None,
      })
      .await;

    let c = MethodContext::with_catalog(ShutdownToken::new(), catalog);
    let resp = dispatch_request(&c, Request::new(1, "list_models", None)).await;
    assert!(resp.error.is_none());
    let body = resp.result.expect("list_models result body");
    let models = body
      .get("models")
      .and_then(Value::as_array)
      .expect("models array");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["path"], json!("/m/seed.gguf"));
    assert_eq!(models[0]["source"], json!("huggingface"));
    assert_eq!(models[0]["metadata"]["quant"], json!("Q4_K"));
  }

  #[tokio::test]
  async fn list_models_returns_empty_array_when_catalog_is_empty() {
    let resp = dispatch_request(&ctx(), Request::new(1, "list_models", None)).await;
    let body = resp.result.expect("result");
    assert_eq!(body["models"], json!([]));
  }

  #[tokio::test]
  async fn wrong_jsonrpc_version_returns_invalid_request() {
    let req = Request {
      jsonrpc: "1.0".into(),
      id: Some(json!(1)),
      method: "ping".into(),
      params: None,
    };
    let resp = dispatch_request(&ctx(), req).await;
    let err = resp.error.expect("wrong version must error");
    assert_eq!(err.code, ErrorCode::InvalidRequest.as_i32());
  }

  #[tokio::test]
  async fn start_model_without_launch_env_returns_internal_error() {
    let c = ctx();
    let req = Request::new(
      1,
      "start_model",
      Some(json!({"model_path": "/nonexistent.gguf"})),
    );
    let resp = dispatch_request(&c, req).await;
    let err = resp.error.expect("must error without launch env");
    assert_eq!(err.code, ErrorCode::InternalError.as_i32());
  }

  #[tokio::test]
  async fn favorite_add_with_unreadable_path_returns_invalid_params() {
    // No GGUF at this path → header-read fails → InvalidParams with
    // an actionable message naming the path.
    let c = ctx();
    let req = Request::new(
      1,
      "favorite_add",
      Some(json!({"model_path": "/no/such/path-9f3a.gguf"})),
    );
    let resp = dispatch_request(&c, req).await;
    let err = resp.error.expect("missing path must error");
    assert_eq!(err.code, ErrorCode::InvalidParams.as_i32());
    assert!(
      err.message.contains("/no/such/path-9f3a.gguf"),
      "error message should name the missing path: {}",
      err.message
    );
  }

  #[tokio::test]
  async fn favorite_list_returns_empty_array_when_state_is_empty() {
    let c = ctx();
    let resp = dispatch_request(&c, Request::new(1, "favorite_list", None)).await;
    let body = resp.result.expect("favorite_list result body");
    assert_eq!(body["favorites"], json!([]));
  }

  #[tokio::test]
  async fn stop_external_refuses_pid_not_in_external_snapshot() {
    let c = ctx();
    let resp = dispatch_request(
      &c,
      Request::new(1, "stop_external", Some(json!({"pid": 999_999_999u32}))),
    )
    .await;
    let err = resp
      .error
      .expect("unknown external PID must reject — safety guard");
    assert_eq!(err.code, ErrorCode::InvalidParams.as_i32());
    assert!(
      err.message.contains("999999999"),
      "error must name the rejected PID, got: {}",
      err.message
    );
  }

  #[tokio::test]
  async fn status_includes_daemon_health_block() {
    let c = ctx();
    let resp = dispatch_request(&c, Request::new(1, "status", None)).await;
    let body = resp.result.expect("status result");
    let daemon = body
      .get("daemon")
      .expect("status must include daemon health block");
    assert!(daemon["pid"].is_number());
    assert!(daemon["uptime_seconds"].is_number());
    assert_eq!(daemon["active_connections"], json!(0));
  }

  #[tokio::test]
  async fn status_includes_backends_block() {
    let c = ctx();
    let resp = dispatch_request(&c, Request::new(1, "status", None)).await;
    let body = resp.result.expect("status result");
    let backends = body
      .get("backends")
      .and_then(|v| v.as_array())
      .expect("status must include a backends array (R3/R16)");
    let ids: Vec<&str> = backends
      .iter()
      .filter_map(|b| b.get("id").and_then(|v| v.as_str()))
      .collect();
    assert!(
      ids.contains(&"llamacpp"),
      "backends must list llamacpp: {ids:?}"
    );
    assert!(
      ids.contains(&"lemonade"),
      "backends must list lemonade: {ids:?}"
    );
    // Each row carries the R16 fields; llama.cpp always offers CPU.
    let llama = backends
      .iter()
      .find(|b| b["id"] == "llamacpp")
      .expect("llamacpp row");
    assert!(llama["installed"].is_boolean());
    assert_eq!(llama["lifecycle"], json!("process_per_model"));
    let accel: Vec<&str> = llama["accelerators"]
      .as_array()
      .unwrap()
      .iter()
      .filter_map(|v| v.as_str())
      .collect();
    assert!(accel.contains(&"cpu"), "llama.cpp floor is cpu: {accel:?}");
    // The Lemonade row is a managed-multiplexer offering cpu+npu.
    let lemon = backends
      .iter()
      .find(|b| b["id"] == "lemonade")
      .expect("lemonade row");
    assert!(lemon["installed"].is_boolean());
    assert_eq!(lemon["lifecycle"], json!("managed_multiplexer"));
    let lacc: Vec<&str> = lemon["accelerators"]
      .as_array()
      .unwrap()
      .iter()
      .filter_map(|v| v.as_str())
      .collect();
    assert!(lacc.contains(&"npu"), "lemonade offers npu: {lacc:?}");
  }

  #[tokio::test]
  async fn status_omits_proxy_block_when_cell_is_absent() {
    // Catalog-only contexts (`MethodContext::new`) leave
    // `proxy_status` as `None`. The wire shape must omit the
    // `proxy` field entirely so the pre-Unit-5 status fixture stays
    // byte-identical — callers that don't surface a proxy don't get
    // a confusing `"proxy": null` blob either.
    let c = ctx();
    let resp = dispatch_request(&c, Request::new(1, "status", None)).await;
    let body = resp.result.expect("status result");
    assert!(
      body.get("proxy").is_none(),
      "proxy block must be absent when no cell is attached: {body}"
    );
  }

  #[tokio::test]
  async fn status_emits_proxy_listening_block() {
    use crate::proxy;
    use std::net::SocketAddr;
    let addr: SocketAddr = "127.0.0.1:11434".parse().unwrap();
    let cell = proxy::server::new_status_cell();
    *cell.write().unwrap() = proxy::ProxyStatus::Listening {
      addr,
      auth_enforced: false,
    };
    let c = MethodContext::new(ShutdownToken::new()).with_proxy_status(cell);
    let resp = dispatch_request(&c, Request::new(1, "status", None)).await;
    let body = resp.result.expect("status result");
    let proxy = body.get("proxy").expect("proxy block present");
    assert_eq!(proxy["enabled"], json!(true));
    assert_eq!(proxy["listen"], json!("127.0.0.1:11434"));
    assert_eq!(proxy["host"], json!("127.0.0.1"));
    assert_eq!(proxy["status"], json!("listening"));
    assert_eq!(proxy["auth"], json!("none"));
    assert_eq!(proxy["bind_error"], Value::Null);
  }

  #[tokio::test]
  async fn status_listening_reports_auth_enforced_and_lan_host() {
    use crate::proxy;
    use std::net::SocketAddr;
    let addr: SocketAddr = "0.0.0.0:11434".parse().unwrap();
    let cell = proxy::server::new_status_cell();
    *cell.write().unwrap() = proxy::ProxyStatus::Listening {
      addr,
      auth_enforced: true,
    };
    let c = MethodContext::new(ShutdownToken::new()).with_proxy_status(cell);
    let resp = dispatch_request(&c, Request::new(1, "status", None)).await;
    let proxy = resp.result.expect("status result");
    let proxy = proxy.get("proxy").expect("proxy block present");
    assert_eq!(proxy["host"], json!("0.0.0.0"));
    assert_eq!(proxy["auth"], json!("enforced"));
  }

  /// A delegated-lemonade snapshot the way `start_delegated_lemonade`
  /// persists one: Backend identity + the synthetic `lemonade://` path.
  fn lemonade_running_snapshot(
    name: &str,
    port: u16,
  ) -> crate::daemon::state_store::RunningSnapshot {
    crate::daemon::state_store::RunningSnapshot {
      id: crate::backend::identity::ModelIdentity::Backend(
        crate::backend::identity::BackendModelId {
          backend: crate::backend::lemonade::LEMONADE_BACKEND_ID.to_string(),
          name: name.to_string(),
        },
      ),
      pid: 0,
      port,
      started_at: 0,
      params: LaunchParams::new(
        PathBuf::from(format!("lemonade://{name}")),
        LaunchMode::Chat,
      ),
      actuals: Default::default(),
    }
  }

  #[tokio::test]
  async fn status_omits_delegated_lemonade_rows_without_umbrella() {
    // A snapshot with no registered umbrella is an unreachable leftover
    // (umbrella crashed / was stopped): emitting a row for it would
    // offer a stop affordance against nothing. The happy path (umbrella
    // up → rows emitted) is covered in `lemonade_umbrella_test.rs`.
    let c = ctx();
    c.state
      .mutate(|s| s.running.push(lemonade_running_snapshot("Qwen-X", 13305)))
      .await;
    let resp = dispatch_request(&c, Request::new(1, "status", None)).await;
    let body = resp.result.expect("status result");
    let models = body["models"].as_array().expect("models array");
    assert!(
      !models.iter().any(|m| m["launch_id"]
        .as_str()
        .unwrap_or("")
        .starts_with("lemonade:")),
      "no delegated rows without a registered umbrella: {models:?}"
    );
  }

  #[tokio::test]
  async fn stop_delegated_lemonade_clears_snapshot_even_without_umbrella() {
    // The umbrella is gone but the snapshot lingers (e.g. it crashed):
    // the row must still be clearable — the unload is best-effort, the
    // bookkeeping removal is the contract.
    let c = ctx();
    c.state
      .mutate(|s| s.running.push(lemonade_running_snapshot("Qwen-X", 13305)))
      .await;
    let resp = dispatch_request(
      &c,
      Request::new(
        1,
        "stop_model",
        Some(json!({"launch_id": "lemonade:Qwen-X"})),
      ),
    )
    .await;
    let body = resp.result.expect("delegated stop must succeed");
    assert_eq!(body["state"]["state"], json!("stopped"));
    let still_there = c
      .state
      .snapshot()
      .await
      .running
      .iter()
      .any(|r| lemonade_snapshot_id(r).is_some());
    assert!(!still_there, "snapshot must be dropped");
    // Second stop: the row is unknown now — same error a bogus
    // supervised launch_id gets.
    let second = dispatch_request(
      &c,
      Request::new(
        2,
        "stop_model",
        Some(json!({"launch_id": "lemonade:Qwen-X"})),
      ),
    )
    .await;
    let err = second.error.expect("double-stop must error");
    assert_eq!(err.code, ErrorCode::InvalidParams.as_i32());
    assert!(err.message.contains("lemonade:Qwen-X"));
  }

  #[tokio::test]
  async fn status_emits_proxy_refused_insecure_block() {
    use crate::proxy;
    use std::net::SocketAddr;
    let addr: SocketAddr = "0.0.0.0:11434".parse().unwrap();
    let cell = proxy::server::new_status_cell();
    *cell.write().unwrap() = proxy::ProxyStatus::RefusedInsecure { addr };
    let c = MethodContext::new(ShutdownToken::new()).with_proxy_status(cell);
    let resp = dispatch_request(&c, Request::new(1, "status", None)).await;
    let proxy = resp.result.expect("status result");
    let proxy = proxy.get("proxy").expect("proxy block present");
    assert_eq!(proxy["status"], json!("refused_insecure"));
    assert_eq!(proxy["auth"], json!("required"));
    assert_eq!(proxy["host"], json!("0.0.0.0"));
    assert!(
      proxy["bind_error"]
        .as_str()
        .unwrap()
        .contains("--insecure-no-auth"),
      "refused_insecure must explain the fix: {proxy}"
    );
  }

  #[tokio::test]
  async fn status_emits_proxy_disabled_block() {
    use crate::proxy;
    let cell = proxy::server::new_status_cell();
    // `new_status_cell` already seeds `Disabled`.
    let c = MethodContext::new(ShutdownToken::new()).with_proxy_status(cell);
    let resp = dispatch_request(&c, Request::new(1, "status", None)).await;
    let body = resp.result.expect("status result");
    let proxy = body.get("proxy").expect("proxy block present");
    assert_eq!(proxy["enabled"], json!(false));
    assert_eq!(proxy["listen"], Value::Null);
    assert_eq!(proxy["status"], json!("disabled"));
    assert_eq!(proxy["bind_error"], Value::Null);
  }

  #[tokio::test]
  async fn status_emits_proxy_port_in_use_block() {
    use crate::proxy;
    use std::net::SocketAddr;
    let addr: SocketAddr = "127.0.0.1:11434".parse().unwrap();
    let cell = proxy::server::new_status_cell();
    *cell.write().unwrap() = proxy::ProxyStatus::PortInUse { addr };
    let c = MethodContext::new(ShutdownToken::new()).with_proxy_status(cell);
    let resp = dispatch_request(&c, Request::new(1, "status", None)).await;
    let body = resp.result.expect("status result");
    let proxy = body.get("proxy").expect("proxy block present");
    assert_eq!(proxy["enabled"], json!(true));
    assert_eq!(proxy["listen"], json!("127.0.0.1:11434"));
    assert_eq!(proxy["status"], json!("port_in_use"));
    // PortInUse is its own discriminator; no parallel bind_error
    // string — the wire shape pins this so parsers don't have to
    // double-check.
    assert_eq!(proxy["bind_error"], Value::Null);
  }

  #[tokio::test]
  async fn status_emits_proxy_unbound_block_with_bind_error() {
    use crate::proxy;
    use std::net::SocketAddr;
    let addr: SocketAddr = "127.0.0.1:80".parse().unwrap();
    let cell = proxy::server::new_status_cell();
    *cell.write().unwrap() = proxy::ProxyStatus::Unbound {
      addr,
      bind_error: "permission denied".to_string(),
    };
    let c = MethodContext::new(ShutdownToken::new()).with_proxy_status(cell);
    let resp = dispatch_request(&c, Request::new(1, "status", None)).await;
    let body = resp.result.expect("status result");
    let proxy = body.get("proxy").expect("proxy block present");
    assert_eq!(proxy["enabled"], json!(true));
    assert_eq!(proxy["listen"], json!("127.0.0.1:80"));
    assert_eq!(proxy["status"], json!("unbound"));
    assert_eq!(proxy["bind_error"], json!("permission denied"));
  }

  #[tokio::test]
  async fn presets_save_with_empty_name_rejects() {
    let c = ctx();
    let req = Request::new(
      1,
      "presets_save",
      Some(json!({"model_path": "/m/a.gguf", "name": ""})),
    );
    let resp = dispatch_request(&c, req).await;
    let err = resp.error.expect("empty name must error");
    assert_eq!(err.code, ErrorCode::InvalidParams.as_i32());
    assert!(
      err.message.to_lowercase().contains("preset name"),
      "got: {}",
      err.message
    );
  }

  #[tokio::test]
  async fn lemonade_start_without_binary_releases_reserved_port() {
    use crate::config::loader::PortRange;
    use crate::gguf::test_fixtures::build_minimal_gguf;
    use crate::launch::params::BackendChoice;

    // A real (minimal) GGUF on disk so `start_model_inner` clears header
    // resolution and reaches the backend-selection seam.
    let dir = tempfile::tempdir().expect("tempdir");
    let model_path = dir.path().join("tiny.gguf");
    std::fs::write(&model_path, build_minimal_gguf("llama")).expect("write gguf");

    // A single-port range on a probe-clear port. Find one the allocator
    // accepts (tolerates TIME_WAIT), then release it so the run under test
    // starts from an empty reservation set.
    let registry = SupervisorRegistry::new();
    let mut found = None;
    for _ in 0..16 {
      let l = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port");
      let p = l.local_addr().unwrap().port();
      drop(l);
      let range = PortRange { start: p, end: p };
      if registry.reserve_port(None, &[], &range).await.is_ok() {
        registry.release_reserved_port(p).await;
        found = Some(p);
        break;
      }
    }
    let port = found.expect("at least one of 16 attempts lands on a probe-clear port");
    let range = PortRange {
      start: port,
      end: port,
    };

    let env = LaunchEnv {
      // Never spawned on this path — the managed-multiplexer arm errors out
      // before any process launch.
      binary: PathBuf::from("/nonexistent/llama-server"),
      port_range: range,
      log_dir: dir.path().to_path_buf(),
      probe: ProbeOptions::default(),
      arch_defaults: Default::default(),
      device_catalog: Arc::new(RwLock::new(Vec::new())),
      default_launch_mode: Default::default(),
      fit_ctx_floor: 16384,
      strict_fit: false,
    };

    // Lemonade enabled but pointed at a binary that does not exist. The
    // explicit-`binary` branch never falls back to PATH, so resolution is
    // deterministically `None` even on a host that has a real `lemond`
    // installed — the test can't be fooled by the dev machine's PATH.
    let ctx = MethodContext::new(ShutdownToken::new())
      .with_supervisors(registry)
      .with_launch_env(env)
      .with_lemonade(LemonadeConfig {
        enabled: true,
        binary: Some(PathBuf::from("/nonexistent/lemond-xyz")),
        port: 13305,
      });

    let parsed = StartParams {
      model_path,
      // Force the managed-multiplexer seam: an explicit Lemonade override
      // outranks the GGUF identity rule.
      backend: Some(BackendChoice::Lemonade),
      ..Default::default()
    };

    // `StartedLaunch` (the Ok variant) isn't `Debug`, so match rather than
    // `expect_err`.
    let err = match start_model_inner(
      &ctx,
      parsed,
      crate::daemon::supervisor::LaunchOrigin::Manual,
    )
    .await
    {
      Ok(_) => panic!("unresolvable lemond binary must error"),
      Err(e) => e,
    };
    assert_eq!(err.code, ErrorCode::InvalidParams.as_i32());
    assert!(
      err.message.contains("lemond"),
      "error should name the missing lemond binary, got: {}",
      err.message
    );

    // The reservation must have been released: the single-port range is
    // allocatable again only if `start_model_inner` dropped its hold on the
    // error path (otherwise the range is exhausted and this errors).
    let reclaimed = ctx
      .supervisors
      .reserve_port(None, &[], &range)
      .await
      .expect("reserved port must be released on the lemonade-unavailable error path");
    assert_eq!(reclaimed, port);
  }
}
