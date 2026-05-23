//! Per-request dispatch. `route` is the body of the `service_fn`
//! closure each hyper connection runs — a flat `match` over
//! `(method, path)` for the routes the proxy answers, mirroring the
//! style of [`crate::ipc::methods::dispatch_request`].
//!
//! The route table covers two surfaces:
//!
//! - **OpenAI compat** (`/v1/...`): `/v1/models`, `/v1/chat/completions`,
//!   `/v1/completions`, `/v1/embeddings`, `/v1/rerank`. This is the
//!   primary surface — any agent that speaks the OpenAI REST shape
//!   drives every discovered model through one stable URL here.
//! - **Ollama-discovery compat** (`/api/...`, Tier 1): `/api/tags`,
//!   `/api/version`, `/api/ps`, `/api/show`. Read-only projections
//!   of the catalog + supervisor registry into the Ollama wire shape,
//!   added so Ollama-shape discovery libraries (the `ollama-python`
//!   default path, `OLLAMA_HOST` env probes, IDE plugins) recognise
//!   llamastash as Ollama-compatible and fall through to the OpenAI
//!   compat endpoints for actual inference. Tier 2 (`/api/chat`,
//!   `/api/generate`, `/api/embed`) is tracked under TODO §R2.

use std::error::Error as StdError;
use std::sync::Arc;

use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use serde_json::json;

use super::forward;
use super::ollama_compat::{
  digest_for_path, ModelDetails, PsModel, PsResponse, ShowRequest, ShowResponse, TagModel,
  TagsResponse, VersionResponse, FAR_FUTURE_EXPIRY, UNKNOWN_MTIME,
};
use super::openai::{ErrorObject, ErrorResponse, ModelList, ModelObject};
use super::route::{self, BodyError as RouteBodyError, RouteDecision};
use super::state::ProxyState;
use crate::cli::resolve::{resolve_model_with_candidates, CatalogRow, ResolveError};
use crate::daemon::supervisor::ManagedState;
use crate::discovery::DiscoveredModel;
use crate::gguf::metadata::{ModeHint, ModelMetadata};

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

  // Route table — OpenAI compat (`/v1/...`) is the primary surface;
  // Ollama compat (`/api/...`, Tier 1) is the discovery surface so
  // Ollama-shape clients recognise the proxy.
  match (&method, path.as_str()) {
    (&Method::GET, "/health") => health(state).await,
    (&Method::GET, "/v1/models") => list_models(state).await,
    (&Method::POST, "/v1/chat/completions") => forward_request(state, req).await,
    (&Method::POST, "/v1/completions") => forward_request(state, req).await,
    (&Method::POST, "/v1/embeddings") => forward_request(state, req).await,
    (&Method::POST, "/v1/rerank") => forward_request(state, req).await,
    // Ollama-compat Tier 1: discovery-only endpoints.
    (&Method::GET, "/api/tags") => ollama_tags(state).await,
    (&Method::GET, "/api/version") => ollama_version(),
    (&Method::GET, "/api/ps") => ollama_ps(state).await,
    (&Method::POST, "/api/show") => ollama_show(state, req).await,
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
  // Both the Ready and NotRunning arms need the same inbound bundle;
  // build it once before the match so the two arms stay short and the
  // forward path sees one canonical shape.
  let inbound = forward::InboundRequest {
    method,
    uri,
    headers,
    body_bytes: parsed.bytes,
  };
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
        inbound,
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
    } => route::handle_not_running(&state, inbound, requested_model, *resolved_row, arch).await,
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
  // Contract: this handler always returns 200 when the listener is
  // bound — even if zero models are Ready. The status of the
  // listener itself (Disabled / Listening / PortInUse / Unbound) is
  // surfaced via the IPC `status.proxy` block, not via the HTTP
  // health probe. If a future caller wants a real "is the proxy
  // serving usefully" signal (e.g. for a systemd readiness gate or a
  // load-balancer probe), introduce a second endpoint rather than
  // overloading `/health` — agents that already pin against 200 on
  // this path would break otherwise. See
  // docs/plans/2026-05-21-001-feat-proxy-router-plan.md (Key
  // Decisions: /health always-200).
  //
  // `models_loaded` filters the supervisor snapshot to entries
  // currently in `ManagedState::Ready`. `models_discovered` is the
  // catalog length — discovery surfaces every row, even parse-error
  // rows, so this matches what `/v1/models` returns.
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

// === Ollama-compat handlers ============================================
//
// Tier 1: read-only discovery endpoints. All four share `model_id_for`
// with the OpenAI compat surface so a model has a single user-visible
// identifier no matter which API a client probes against.

/// `GET /api/tags` — list every discovered model in Ollama shape,
/// sorted alphabetically by `name` (parity with `/v1/models`). Empty
/// catalog returns `{"models":[]}` (not a 404).
async fn ollama_tags(state: Arc<ProxyState>) -> ProxyResponse {
  let snap = state.ctx.catalog.snapshot().await;
  let mut models: Vec<TagModel> = snap.iter().map(ollama_tag_from_discovered).collect();
  models.sort_by(|a, b| a.name.cmp(&b.name));
  let body = TagsResponse { models };
  let bytes = serde_json::to_vec(&body).expect("json encoding of fixed shape");
  Ok(json_response(StatusCode::OK, bytes))
}

/// `GET /api/version` — daemon build version. Same value
/// `status.daemon.build` surfaces; cargo's `CARGO_PKG_VERSION` at
/// compile time.
fn ollama_version() -> ProxyResponse {
  let body = VersionResponse {
    version: env!("CARGO_PKG_VERSION"),
  };
  let bytes = serde_json::to_vec(&body).expect("json encoding of fixed shape");
  Ok(json_response(StatusCode::OK, bytes))
}

/// `GET /api/ps` — currently-Ready supervisors projected into Ollama's
/// running-list shape. Empty when no model is Ready.
async fn ollama_ps(state: Arc<ProxyState>) -> ProxyResponse {
  let sup_snap = state.ctx.supervisors.snapshot().await;
  let cat_snap = state.ctx.catalog.snapshot().await;
  // Index the catalog by canonical path so each Ready supervisor can
  // look up its metadata without re-walking the catalog. Same shape as
  // route::collect_fallback_candidates.
  let mut by_path: std::collections::HashMap<String, &DiscoveredModel> =
    std::collections::HashMap::with_capacity(cat_snap.len());
  for m in cat_snap.iter() {
    by_path.insert(m.path.to_string_lossy().into_owned(), m);
  }
  let mut models: Vec<PsModel> = Vec::new();
  for (_launch_id, sup) in sup_snap.into_iter() {
    if !matches!(sup.state().await, ManagedState::Ready) {
      continue;
    }
    let id = sup.id().clone();
    let path_key = id.path.to_string_lossy().into_owned();
    let name = if let Some(d) = by_path.get(&path_key) {
      model_id_for(d)
    } else {
      crate::util::paths::model_display_name(&id.path)
    };
    let details = by_path
      .get(&path_key)
      .map(|d| ollama_details_from_metadata(d.metadata.as_ref()))
      .unwrap_or_else(ollama_details_unknown);
    let size = by_path
      .get(&path_key)
      .and_then(|d| d.metadata.as_ref())
      .and_then(|m| m.weights_bytes)
      .unwrap_or(0);
    models.push(PsModel {
      name: name.clone(),
      model: name,
      size,
      // Path-derived digest, identical to what /api/tags emits for
      // the same model — see `ollama_compat::digest_for_path`.
      digest: digest_for_path(&id.path),
      details,
      expires_at: FAR_FUTURE_EXPIRY.to_string(),
      size_vram: 0,
    });
  }
  models.sort_by(|a, b| a.name.cmp(&b.name));
  let body = PsResponse { models };
  let bytes = serde_json::to_vec(&body).expect("json encoding of fixed shape");
  Ok(json_response(StatusCode::OK, bytes))
}

/// `POST /api/show` — return per-model metadata for the model named
/// in the request body. Body shape: `{"model": "<name>"}` (or `"name"`
/// for older clients — both spellings accepted). Resolution rules
/// match the OpenAI compat surface — same fuzzy matcher as
/// `body.model` on `/v1/...` so a name that works on one endpoint
/// works on the other.
async fn ollama_show(state: Arc<ProxyState>, req: Request<Incoming>) -> ProxyResponse {
  let (_method, _uri, _headers, body) = forward::deconstruct(req);
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
    Err(RouteBodyError::Malformed { message }) | Err(RouteBodyError::Read { message }) => {
      return error_response(StatusCode::BAD_REQUEST, "invalid_request", &message);
    }
  };
  // Re-parse the body bytes into the Ollama-shape ShowRequest. The
  // proxy's `JustModel` peek picked out `body.model`; for /api/show we
  // also tolerate `body.name`, so we re-parse rather than reuse
  // `parsed.model`.
  let show_req: ShowRequest = if parsed.bytes.is_empty() {
    ShowRequest {
      model: None,
      name: None,
    }
  } else {
    match serde_json::from_slice(&parsed.bytes) {
      Ok(r) => r,
      Err(e) => {
        return error_response(
          StatusCode::BAD_REQUEST,
          "invalid_request",
          &format!("request body is not valid JSON: {e}"),
        );
      }
    }
  };
  let reference = match show_req.reference() {
    Some(r) => r.to_string(),
    None => {
      return error_with_code(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        "the `model` field is required",
        "model_required",
        Some("model"),
      );
    }
  };
  // Resolve against the catalog using the same matcher the OpenAI
  // compat surface uses, so identical names work across both APIs.
  let snap = state.ctx.catalog.snapshot().await;
  let rows: Vec<CatalogRow> = snap
    .iter()
    .map(catalog_row_for_resolver)
    .collect::<Vec<_>>();
  match resolve_model_with_candidates(&rows, &reference) {
    Ok(resolved) => {
      // Re-find the DiscoveredModel for the resolved path so we have
      // the live metadata. The resolver returns a CatalogRow clone;
      // metadata projection wants the source DiscoveredModel for the
      // ModelId.
      let discovered = snap
        .iter()
        .find(|m| m.path.to_string_lossy() == resolved.path)
        .cloned();
      let response = build_show_response(discovered.as_ref(), &resolved);
      let bytes = serde_json::to_vec(&response).expect("json encoding of fixed shape");
      Ok(json_response(StatusCode::OK, bytes))
    }
    Err(ResolveError::Empty) | Err(ResolveError::None) => error_with_matches(
      StatusCode::NOT_FOUND,
      "model_not_found",
      &format!("{reference} not found"),
      Vec::<String>::new(),
    ),
    Err(ResolveError::Many(candidates)) => {
      let names: Vec<String> = candidates.iter().map(|r| r.name()).collect();
      let n = names.len();
      error_with_matches(
        StatusCode::BAD_REQUEST,
        "ambiguous_model",
        &format!(
          "`{reference}` matched {n} models; refine the reference (full path or unique substring)"
        ),
        names,
      )
    }
  }
}

/// Build an Ollama `details` block from optional GGUF metadata. Empty
/// strings stand in for fields we can't fill (parse-error rows, models
/// missing arch/parameter-label metadata).
fn ollama_details_from_metadata(meta: Option<&ModelMetadata>) -> ModelDetails {
  match meta {
    Some(m) => {
      let family = m.arch.clone().unwrap_or_default();
      let families = if family.is_empty() {
        Vec::new()
      } else {
        vec![family.clone()]
      };
      ModelDetails {
        parent_model: String::new(),
        format: "gguf",
        family,
        families,
        parameter_size: m.parameter_label.clone().unwrap_or_default(),
        quantization_level: m.quant.label().to_string(),
      }
    }
    None => ollama_details_unknown(),
  }
}

/// Empty-shape details for catalog rows where the GGUF header parse
/// failed.
fn ollama_details_unknown() -> ModelDetails {
  ModelDetails {
    parent_model: String::new(),
    format: "gguf",
    family: String::new(),
    families: Vec::new(),
    parameter_size: String::new(),
    quantization_level: "Unknown".to_string(),
  }
}

/// Project a [`DiscoveredModel`] onto an Ollama `/api/tags` row.
fn ollama_tag_from_discovered(m: &DiscoveredModel) -> TagModel {
  let name = model_id_for(m);
  let size = m
    .metadata
    .as_ref()
    .and_then(|md| md.weights_bytes)
    .unwrap_or(0);
  // Path-derived digest, identical to what /api/ps emits for the
  // same model. Parse-error rows hash their path too — clients see
  // a stable, comparable value rather than a special-case sentinel.
  let digest = digest_for_path(&m.path);
  TagModel {
    name: name.clone(),
    model: name,
    modified_at: UNKNOWN_MTIME.to_string(),
    size,
    digest,
    details: ollama_details_from_metadata(m.metadata.as_ref()),
  }
}

/// Project a [`DiscoveredModel`] onto the `CatalogRow` shape the
/// resolver expects. Inline equivalent of
/// `proxy::route::catalog_row_from_discovered`; kept private here so
/// the show handler doesn't depend on the route module's internals.
fn catalog_row_for_resolver(m: &DiscoveredModel) -> CatalogRow {
  let path = m.path.to_string_lossy().into_owned();
  let parent = m.parent.to_string_lossy().into_owned();
  let arch = m.metadata.as_ref().and_then(|md| md.arch.clone());
  let quant = m.metadata.as_ref().map(|md| md.quant.label().to_string());
  let native_ctx = m.metadata.as_ref().and_then(|md| md.native_ctx);
  let parameter_label = m
    .metadata
    .as_ref()
    .and_then(|md| md.parameter_label.clone());
  let weights_bytes = m.metadata.as_ref().and_then(|md| md.weights_bytes);
  CatalogRow {
    path,
    model_id: None,
    parent,
    source: m.source.label().to_string(),
    arch,
    quant,
    native_ctx,
    mode_hint: None,
    parameter_label,
    weights_bytes,
    display_label: m.display_label.clone(),
    parse_error: m.parse_error.clone(),
  }
}

/// Build the `/api/show` response from a resolved catalog row +
/// optional source discovery row (for the `model_info` slot).
fn build_show_response(discovered: Option<&DiscoveredModel>, row: &CatalogRow) -> ShowResponse {
  let template = discovered
    .and_then(|d| d.metadata.as_ref())
    .and_then(|m| m.chat_template.clone())
    .unwrap_or_default();
  let details = ollama_details_from_metadata(discovered.and_then(|d| d.metadata.as_ref()));
  let model_info = build_model_info(discovered, row);
  let capabilities = capabilities_for(discovered);
  ShowResponse {
    modelfile: String::new(),
    parameters: String::new(),
    template,
    details,
    model_info,
    capabilities,
  }
}

/// Build the `model_info` flat map. Mirrors the GGUF metadata slots
/// Ollama clients commonly read.
fn build_model_info(
  discovered: Option<&DiscoveredModel>,
  row: &CatalogRow,
) -> serde_json::Map<String, serde_json::Value> {
  let mut map = serde_json::Map::new();
  if let Some(arch) = &row.arch {
    map.insert("general.architecture".into(), json!(arch));
  }
  if let Some(meta) = discovered.and_then(|d| d.metadata.as_ref()) {
    if let Some(n) = meta.total_parameters {
      map.insert("general.parameter_count".into(), json!(n));
    }
    if let Some(label) = &meta.parameter_label {
      map.insert("general.parameter_label".into(), json!(label));
    }
    if let Some(ctx) = meta.native_ctx {
      map.insert("general.context_length".into(), json!(ctx));
    }
    if let Some(tok) = &meta.tokenizer_kind {
      map.insert("tokenizer.ggml.model".into(), json!(tok));
    }
    map.insert("general.quantization".into(), json!(meta.quant.label()));
  }
  map
}

/// Derive the Ollama-shape `capabilities` list from the discovered
/// model's mode hint. Ollama uses `"completion"` for chat/text models,
/// `"embedding"` for embedding models, `"tools"` for tool-use-capable
/// chat models. llamastash maps from [`ModeHint`]:
///   - `Chat` → `["completion"]`
///   - `Embedding` → `["embedding"]`
///   - `Rerank` → `["rerank"]` (Ollama doesn't define this; emitted
///     for parity with our own `/v1/rerank` surface)
///   - `Unknown` → empty (catalog has no hint)
fn capabilities_for(discovered: Option<&DiscoveredModel>) -> Vec<&'static str> {
  let mode = discovered
    .and_then(|d| d.metadata.as_ref())
    .map(|m| m.mode_hint)
    .unwrap_or(ModeHint::Unknown);
  match mode {
    ModeHint::Chat => vec!["completion"],
    ModeHint::Embedding => vec!["embedding"],
    ModeHint::Rerank => vec!["rerank"],
    ModeHint::Unknown => Vec::new(),
  }
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
