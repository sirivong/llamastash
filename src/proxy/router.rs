//! Per-request dispatch. `route` is the body of the `service_fn`
//! closure each hyper connection runs — a flat `match` over
//! `(method, path)` for the six fixed routes the proxy answers,
//! mirroring the style of [`crate::ipc::methods::dispatch_request`].
//!
//! Unit 1 stood up `/health`; Unit 2 adds `/v1/models`. The remaining
//! four arms (`/v1/chat/completions`, `/v1/completions`,
//! `/v1/embeddings`, `/v1/rerank`) stay 501 until Units 3/4 land the
//! resolution + forwarding plumbing.

use std::error::Error as StdError;
use std::sync::Arc;

use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use serde_json::json;

use super::forward;
use super::openai::{ErrorObject, ErrorResponse, ModelList, ModelObject};
use super::route::{self, BodyError as RouteBodyError, RouteDecision};
use super::state::ProxyState;
use crate::daemon::supervisor::ManagedState;
use crate::discovery::DiscoveredModel;

/// The error type our `BoxBody` carries. Unit 3 streams upstream
/// `reqwest::Response::bytes_stream()` chunks through `StreamBody`,
/// so the body alias must accept *some* error type at frame time.
/// Boxed dyn errors are the most flexible choice — non-streaming
/// arms (errors, health, /v1/models) still emit `Infallible`-shaped
/// `Full` bodies and coerce into the wider alias via `map_err`.
pub type BodyError = Box<dyn StdError + Send + Sync>;

/// What every handler returns. `Result<_, hyper::Error>` is the
/// `service_fn` contract; the inner body is boxed so each arm can
/// pick whatever concrete `Body` makes sense without poisoning the
/// outer signature.
pub type ProxyResponse = Result<Response<BoxBody<Bytes, BodyError>>, hyper::Error>;

/// Entry point invoked by the `service_fn` closure. Returns a fully
/// constructed `Response`; the caller hands it back to hyper.
pub async fn route(state: Arc<ProxyState>, req: Request<Incoming>) -> ProxyResponse {
  let method = req.method().clone();
  let path = req.uri().path().to_string();

  // 6-route dispatch table. Unit 3 lights up the four `/v1/...`
  // forwarding arms; Unit 4 layers auto-start + fallback on top.
  match (&method, path.as_str()) {
    (&Method::GET, "/health") => health(state).await,
    (&Method::GET, "/v1/models") => list_models(state).await,
    (&Method::POST, "/v1/chat/completions") => forward_request(state, req).await,
    (&Method::POST, "/v1/completions") => forward_request(state, req).await,
    (&Method::POST, "/v1/embeddings") => forward_request(state, req).await,
    (&Method::POST, "/v1/rerank") => forward_request(state, req).await,
    _ => not_found(),
  }
}

/// Unit 3 pipeline: buffer the body under the 2 MiB cap, extract
/// `body.model`, run the resolver, pick a Ready supervisor, forward.
async fn forward_request(state: Arc<ProxyState>, req: Request<Incoming>) -> ProxyResponse {
  let (method, uri, headers, body) = forward::deconstruct(req);

  let parsed = match route::buffer_and_extract(body).await {
    Ok(p) => p,
    Err(RouteBodyError::TooLarge) => {
      return error_response(
        StatusCode::PAYLOAD_TOO_LARGE,
        "payload_too_large",
        &format!(
          "request body exceeds the {} MiB limit",
          route::BODY_LIMIT_BYTES / (1024 * 1024)
        ),
      );
    }
    Err(RouteBodyError::Malformed { message }) => {
      return error_response(StatusCode::BAD_REQUEST, "invalid_request", &message);
    }
    Err(RouteBodyError::Read { message }) => {
      return error_response(StatusCode::BAD_REQUEST, "invalid_request", &message);
    }
  };

  let decision = route::decide(&state, parsed.model).await;
  match decision {
    RouteDecision::ReadyAt {
      port,
      served_model_id,
      served_model_key,
      fallback,
      fallback_reason,
    } => {
      // MRU touch on the Ready path. Stamped *before* forwarding
      // begins (per the plan's "as it starts forwarding, not on
      // completion" rule) so long-running streams don't delay the
      // timestamp. Direct touch by ModelId avoids the second
      // supervisor snapshot the port-only path used to take.
      state.mru.touch(&served_model_key).await;
      forward::forward_to_upstream(
        &state,
        forward::InboundRequest {
          method,
          uri,
          headers,
          body_bytes: parsed.bytes,
        },
        forward::Target {
          port,
          served_model_id: &served_model_id,
          served_model_key: &served_model_key,
          fallback,
          fallback_reason: fallback_reason.as_deref(),
        },
      )
      .await
    }
    RouteDecision::NotRunning {
      requested_model,
      resolved_row,
      arch,
    } => {
      route::handle_not_running(
        &state,
        forward::InboundRequest {
          method,
          uri,
          headers,
          body_bytes: parsed.bytes,
        },
        requested_model,
        *resolved_row,
        arch,
      )
      .await
    }
    RouteDecision::NotFound { requested_model } => error_with_matches(
      StatusCode::NOT_FOUND,
      "model_not_found",
      &format!("{requested_model} not found"),
      Vec::<String>::new(),
    ),
    RouteDecision::Ambiguous {
      requested_model,
      candidates,
    } => {
      let message = format!(
        "`{requested_model}` matched {n} models; refine the reference (full path or unique substring)",
        n = candidates.len()
      );
      error_with_matches(
        StatusCode::BAD_REQUEST,
        "ambiguous_model",
        &message,
        candidates,
      )
    }
    RouteDecision::ModelRequired => error_with_code(
      StatusCode::BAD_REQUEST,
      "invalid_request",
      "the `model` field is required",
      "model_required",
      Some("model"),
    ),
  }
}

async fn health(state: Arc<ProxyState>) -> ProxyResponse {
  // `models_loaded` filters the supervisor snapshot to entries
  // currently in `ManagedState::Ready`. Unit 1 used `len()` as a
  // wire-shape stand-in; Unit 2 promotes it to the real Ready count
  // per R158 / R159. `models_discovered` is the catalog length —
  // discovery surfaces every row, even parse-error rows, so this
  // matches what `/v1/models` returns.
  let models_loaded = count_ready(&state).await;
  let models_discovered = state.ctx.catalog.len().await;
  let body = json!({
    "status": "ok",
    "models_loaded": models_loaded,
    "models_discovered": models_discovered,
  });
  // serde_json::to_vec on a hand-built `Value` cannot fail.
  let bytes = serde_json::to_vec(&body).expect("json encoding of fixed shape");
  Ok(json_response(StatusCode::OK, bytes))
}

/// Count supervisors currently in `ManagedState::Ready`. Each
/// `state()` call acquires a per-supervisor read lock, so the
/// snapshot is a sequence of cheap clones rather than one global
/// lock — consistent with how `status_handler` walks the registry.
async fn count_ready(state: &ProxyState) -> usize {
  let snap = state.ctx.supervisors.snapshot().await;
  let mut ready = 0usize;
  for (_id, model) in snap {
    if matches!(model.state().await, ManagedState::Ready) {
      ready += 1;
    }
  }
  ready
}

/// `GET /v1/models` — list every discovered model in OpenAI shape,
/// sorted alphabetically by `id`. Empty catalog returns
/// `{"object":"list","data":[]}` (not a 404, not an error).
async fn list_models(state: Arc<ProxyState>) -> ProxyResponse {
  let snap = state.ctx.catalog.snapshot().await;
  let mut rows: Vec<ModelObject> = snap
    .iter()
    .map(|m| ModelObject::new(model_id_for(m)))
    .collect();
  // ASCII-lexicographic sort: stable, deterministic across runs, and
  // independent of the catalog's underlying BTreeMap key (canonical
  // path) which orders by filesystem layout instead of display name.
  rows.sort_by(|a, b| a.id.cmp(&b.id));
  let list = ModelList::new(rows);
  let bytes = serde_json::to_vec(&list).expect("json encoding of fixed shape");
  Ok(json_response(StatusCode::OK, bytes))
}

/// Project a [`DiscoveredModel`] onto the `id` field of an OpenAI
/// `model` object. Rule: `display_label` wins when set (Ollama
/// surfaces `<name>:<tag>` here), otherwise fall back to
/// `path.file_stem()` via [`crate::util::paths::model_display_name`].
/// This matches what the TUI and `llamastash list` show, so the same
/// model identifier appears in every surface.
///
/// Note: `CatalogRow::name()` falls back to `path.file_name()`
/// (basename *with* extension) rather than the file stem. The plan
/// explicitly calls for the stem here so the OpenAI `id` reads
/// cleanly (`qwen2.5-coder` rather than `qwen2.5-coder.gguf`). The
/// resolver's substring matching (used in Unit 3) is tolerant to
/// either form, so this divergence is intentional and bounded.
fn model_id_for(m: &DiscoveredModel) -> String {
  if let Some(label) = &m.display_label {
    return label.clone();
  }
  crate::util::paths::model_display_name(&m.path)
}

fn not_found() -> ProxyResponse {
  error_response(StatusCode::NOT_FOUND, "not_found", "no such route")
}

/// Build an OpenAI-shaped error response from a `(status, type,
/// message)` triple. Centralised so the 404 / Unit 3
/// `model_not_running` arms all emit the same
/// `{"error":{"type":..., "message":...}}` envelope.
pub(crate) fn error_response(status: StatusCode, r#type: &str, message: &str) -> ProxyResponse {
  let body = ErrorResponse {
    error: ErrorObject::new(r#type, message),
  };
  let bytes = serde_json::to_vec(&body).expect("json encoding of fixed shape");
  Ok(json_response(status, bytes))
}

/// Variant of [`error_response`] that stamps `code` (e.g.
/// `"model_required"`) + `param` (e.g. `"model"`). Used by the
/// `invalid_request` arm so OpenAI SDK clients can branch on `code`
/// without parsing `message`.
pub(crate) fn error_with_code(
  status: StatusCode,
  r#type: &str,
  message: &str,
  code: &str,
  param: Option<&str>,
) -> ProxyResponse {
  let mut error = ErrorObject::new(r#type, message).with_code(code);
  if let Some(p) = param {
    error = error.with_param(p);
  }
  let bytes = serde_json::to_vec(&ErrorResponse { error }).expect("json encoding of fixed shape");
  Ok(json_response(status, bytes))
}

/// Variant of [`error_response`] that stamps the candidate-name
/// `matches` list. Used by `model_not_found` (empty list) and
/// `ambiguous_model` (≥ 2 names).
pub(crate) fn error_with_matches<I, S>(
  status: StatusCode,
  r#type: &str,
  message: &str,
  matches: I,
) -> ProxyResponse
where
  I: IntoIterator<Item = S>,
  S: Into<String>,
{
  let error = ErrorObject::new(r#type, message).with_matches(matches);
  let bytes = serde_json::to_vec(&ErrorResponse { error }).expect("json encoding of fixed shape");
  Ok(json_response(status, bytes))
}

/// Build the 503 `launch_failed` envelope used by Unit 4's
/// fallback path when no Ready model is available. The
/// `running: []` field is always present (R155); the message
/// surfaces the supervisor's `cause` so clients see *why* the
/// launch failed.
pub(crate) fn launch_failed_response<I, S>(
  cause: &str,
  running: I,
  requested_model: &str,
) -> ProxyResponse
where
  I: IntoIterator<Item = S>,
  S: Into<String>,
{
  let message =
    format!("auto-start of `{requested_model}` failed and no running model is available: {cause}");
  let error = ErrorObject::new("launch_failed", message).with_running(running);
  let bytes = serde_json::to_vec(&ErrorResponse { error }).expect("json encoding of fixed shape");
  Ok(json_response(StatusCode::SERVICE_UNAVAILABLE, bytes))
}

fn json_response(status: StatusCode, body: Vec<u8>) -> Response<BoxBody<Bytes, BodyError>> {
  let body = full_body(Bytes::from(body));
  Response::builder()
    .status(status)
    .header(hyper::header::CONTENT_TYPE, "application/json")
    .body(body)
    .expect("static headers always parse")
}

/// Wrap an in-memory `Bytes` payload as a `BoxBody` whose error
/// type is the wider `BodyError` alias. `Full`'s native error is
/// `Infallible`; `map_err` widens it (via the `From<Infallible>`
/// blanket impl) so non-streaming arms share the streaming arm's
/// body alias.
fn full_body(bytes: Bytes) -> BoxBody<Bytes, BodyError> {
  Full::new(bytes).map_err(|never| match never {}).boxed()
}
