//! Method dispatch for Unit 2.
//!
//! Unit 2 ships three methods: `ping`, `version`, and `shutdown`. Model
//! methods (list_models, start, stop, …) land in later units; each will
//! add a match arm in `dispatch_request` plus an optional helper on
//! `MethodContext`. Keeping the registry as a `match` (rather than a
//! `HashMap<&str, fn>`) avoids dynamic-dispatch plumbing for what is, in
//! practice, a small fixed set of methods.

use std::{
  sync::{atomic::Ordering, Arc},
  time::Instant,
};

use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

use super::protocol::{ErrorCode, ErrorObject, Request, Response, JSONRPC_VERSION};
use crate::daemon::registry::{LaunchId, SupervisorRegistry};
use crate::daemon::shutdown::ShutdownToken;
use crate::daemon::supervisor::ManagedState;
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
  /// Snapshot of `gpu::probe()` taken at daemon start. `status`
  /// reports it alongside per-model resources so the UI can render
  /// a GPU panel.
  pub gpu: Arc<GpuInfo>,
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
          "pid": std::process::id(),
          "uptime_seconds": uptime_secs,
          "connections": connections,
        }),
      )
    }
    "shutdown" => {
      ctx.shutdown.trigger();
      Response::ok(id, json!({"shutdown": "scheduled"}))
    }
    "list_models" => {
      // Read the latest catalog snapshot. The discovery task keeps
      // this up-to-date; handlers never trigger a scan themselves.
      let body = ctx.catalog.to_list_response().await;
      Response::ok(id, body)
    }
    "status" => Response::ok(id, status_response(ctx).await),
    "stop_model" => match stop_model_handler(ctx, req.params).await {
      Ok(v) => Response::ok(id, v),
      Err(e) => Response::err(id, e),
    },
    "stop_all" => match stop_all_handler(ctx).await {
      Ok(v) => Response::ok(id, v),
      Err(e) => Response::err(id, e),
    },
    "logs_tail" => match logs_tail_handler(ctx, req.params).await {
      Ok(v) => Response::ok(id, v),
      Err(e) => Response::err(id, e),
    },
    other => Response::err(
      id,
      ErrorObject::new(
        ErrorCode::MethodNotFound,
        format!("unknown method: {other}"),
      ),
    ),
  }
}

/// Snapshot every active managed model plus the daemon's GPU info.
/// `status` is read-only; never triggers any state-machine transitions.
async fn status_response(ctx: &MethodContext) -> Value {
  let snap = ctx.supervisors.snapshot().await;
  let mut models: Vec<Value> = Vec::with_capacity(snap.len());
  for (launch_id, model) in snap {
    let state = model.state().await;
    let pid = model.pid().await;
    let ready_at = model.ready_at().await;
    models.push(json!({
      "launch_id": launch_id,
      "id": model.id(),
      "port": model.port(),
      "mode": model.mode().label(),
      "pid": pid,
      "ready_at": ready_at,
      "state": state,
    }));
  }
  json!({
    "models": models,
    "gpu": ctx.gpu.as_ref(),
  })
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

async fn stop_model_handler(
  ctx: &MethodContext,
  params: Option<Value>,
) -> Result<Value, ErrorObject> {
  let parsed: StopParams = parse_params(params)?;
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
  let final_state = model.stop(Duration::from_secs(parsed.grace_secs)).await;
  ctx.supervisors.remove(&parsed.launch_id).await;
  Ok(json!({
    "launch_id": parsed.launch_id,
    "state": final_state,
  }))
}

async fn stop_all_handler(ctx: &MethodContext) -> Result<Value, ErrorObject> {
  let snap = ctx.supervisors.snapshot().await;
  let mut stopped: Vec<Value> = Vec::with_capacity(snap.len());
  for (launch_id, model) in snap {
    let s = model.stop(Duration::from_secs(default_grace_secs())).await;
    ctx.supervisors.remove(&launch_id).await;
    stopped.push(json!({"launch_id": launch_id, "state": s}));
  }
  Ok(json!({"stopped": stopped}))
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
  let tail = model.tail(parsed.lines).await;
  Ok(json!({
    "launch_id": parsed.launch_id,
    "lines": tail,
  }))
}

fn parse_params<T: serde::de::DeserializeOwned>(params: Option<Value>) -> Result<T, ErrorObject> {
  let raw = params.unwrap_or(Value::Null);
  serde_json::from_value(raw)
    .map_err(|e| ErrorObject::new(ErrorCode::InvalidParams, format!("params parse error: {e}")))
}

// Silence the unused-state-import warning when no test exercises it.
#[allow(dead_code)]
const _: fn() = || {
  let _ = std::mem::size_of::<ManagedState>();
};

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

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
          reasoning_hint: None,
          mode_hint: ModeHint::Chat,
        }),
        parse_error: None,
        split_siblings: Vec::new(),
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
}
