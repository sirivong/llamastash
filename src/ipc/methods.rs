//! Method dispatch for the daemon's IPC layer.
//!
//! Keeping the registry as a `match` (rather than a
//! `HashMap<&str, fn>`) avoids dynamic-dispatch plumbing for what is,
//! in practice, a small fixed set of methods.

use std::{ffi::OsString, path::PathBuf, sync::atomic::Ordering, time::Duration};

use serde::Deserialize;
use serde_json::{json, Value};

use super::protocol::{ErrorCode, ErrorObject, Request, Response, JSONRPC_VERSION};
use crate::backend::identity::ModelIdentity;
use crate::daemon::context::MethodContext;
use crate::daemon::launch_service::{compose_and_spawn, LaunchModeWire, StartParams};
use crate::daemon::registry::LaunchId;
use crate::daemon::supervisor::ManagedState;
use crate::gguf::header::{read_path as read_gguf_header, HeaderReadOptions};
use crate::gguf::identity::{compute as compute_model_id, ModelId};
use crate::launch::favorites::FavoriteEntry;
use crate::launch::mode::LaunchMode;
use crate::launch::params::LaunchParams;
use crate::launch::presets::NamedPreset;

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
    "status" => Response::ok(id, crate::ipc::status::status_response(ctx).await),
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
        s.running.retain(|r| r.lemonade_backend_id().is_none());
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
        .retain(|r| r.lemonade_backend_id().map(|b| b.name.as_str()) != Some(name));
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

/// Flatten `ManagedState` to a JSON object whose `state` field is a
/// lowercase string label plus an optional `cause`. Used by
/// `stop_model` / `stop_all` responses and the status rows so every
/// surface reports model state in one shape.
pub(crate) fn flatten_state(state: &ManagedState) -> Value {
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
  // through [`ProcessControl`] so the Windows single-pid path stays
  // in one place rather than a second migration here.
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

/// IPC `start_model` handler — a thin wrapper around
/// [`compose_and_spawn`](crate::daemon::launch_service::compose_and_spawn).
/// Keeps the JSON-RPC plumbing (parse params → call the launch service →
/// JSON-encode response) at the handler boundary so the proxy's
/// auto-start can call the service directly without round-tripping
/// through the dispatcher.
async fn start_model_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: StartParams = parse_params(params)?;
  // IPC clients are user-initiated (TUI Launch, `llamastash start`,
  // bare JSON-RPC). The proxy's auto-start path bypasses this
  // handler and calls `compose_and_spawn` directly with
  // `LaunchOrigin::AutoStart`.
  let started =
    compose_and_spawn(ctx, parsed, crate::daemon::supervisor::LaunchOrigin::Manual).await?;
  let pid = started.model.pid().await;
  Ok(json!({
    "launch_id": started.launch_id,
    "model_id": started.model_id,
    "port": started.port,
    "pid": pid,
    "log_path": started.log_path,
  }))
}

pub(crate) fn resolve_model_id(path: &std::path::Path) -> Result<ModelId, ErrorObject> {
  let (id, _, _) = resolve_model_id_and_arch(path)?;
  Ok(id)
}

/// One-pass GGUF header read that returns both the canonical model id
/// and the architecture string. The launch path calls this so the
/// layered-knob resolver lookup doesn't have to re-read the header to
/// discover the arch. Arch is best-effort: a `None` here just means
/// the `defaults_table` lookup falls back to the `*` row.
pub(crate) fn resolve_model_id_and_arch(
  path: &std::path::Path,
) -> Result<(ModelId, Option<String>, Option<u32>), ErrorObject> {
  let header = read_gguf_header(path, HeaderReadOptions::default()).map_err(|e| {
    ErrorObject::new(
      ErrorCode::InvalidParams,
      format!("could not read GGUF header at {}: {e}", path.display()),
    )
  })?;
  let id = compute_model_id(path, &header.raw);
  let summary = crate::gguf::metadata::summarise(&header.header);
  // Trained context window (`<arch>.context_length`), clamped into u32.
  // Feeds the strict-fit ctx-clamp gate: a `--fit` resolution pinned to
  // the floor is only "degraded" when the model could have gone higher.
  let native_ctx = summary
    .native_ctx
    .map(|n| u32::try_from(n).unwrap_or(u32::MAX));
  Ok((id, summary.arch, native_ctx))
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
    "jinja": p.jinja,
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
  use crate::daemon::shutdown::ShutdownToken;
  use crate::discovery::ModelCatalog;

  fn ctx() -> MethodContext {
    MethodContext::new(ShutdownToken::new())
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
      .any(|r| r.lemonade_backend_id().is_some());
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
}
