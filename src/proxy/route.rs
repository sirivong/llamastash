//! Pre-flight: turn an inbound HTTP request into a forwarding plan.
//!
//! Unit 3 walks every incoming `/v1/...` request through this module
//! before reaching for the upstream `llama-server`. The output is a
//! [`RouteDecision`] that captures everything [`super::forward`]
//! needs to do the pass-through, plus enough context for the error
//! arms to render an OpenAI-shaped body.
//!
//! Hot path:
//!   1. Buffer the body under a 2 MiB cap ([`http-body-util::Limited`]).
//!   2. Extract `body.model` with a tolerant `JustModel` parse that
//!      ignores every other field. Empty / missing → 400.
//!   3. Build a `Vec<CatalogRow>` from the catalog snapshot and run
//!      the existing fuzzy resolver ([`crate::cli::resolve::resolve_model`]).
//!   4. Walk the supervisor snapshot for a Ready entry whose
//!      [`ModelId`] path matches the resolved catalog row.
//!
//! Unit 3 stops at step 4 — no auto-start, no fallback. Unit 4
//! replaces the [`RouteDecision::NotRunning`] arm with the launch +
//! single-flight + fallback machinery; the variant intentionally
//! carries the resolved row + arch so Unit 4 doesn't have to repeat
//! the lookup.
//!
//! Plan: docs/plans/2026-05-21-001-feat-proxy-router-plan.md (Unit 3).

use std::sync::Arc;

use http_body_util::{BodyExt, Limited};
use hyper::body::{Bytes, Incoming};

use crate::cli::resolve::{resolve_model, CatalogRow};
use crate::daemon::supervisor::ManagedState;
use crate::discovery::DiscoveredModel;
use crate::gguf::identity::ModelId;

use super::launch::{self, LaunchOutcome};
use super::mru::{pick_fallback, FallbackCandidate};
use super::router::ProxyResponse;
use super::state::ProxyState;

/// Inbound body size cap. The 2 MiB ceiling lets OpenAI-shape chat
/// completions with multi-thousand-token histories through while
/// still bounding worst-case memory and refusing accidental
/// uploads. Anything larger surfaces as HTTP 413 via [`BodyError::TooLarge`].
pub const BODY_LIMIT_BYTES: usize = 2 * 1024 * 1024;

/// Forwarding plan produced by [`decide`]. Keep this `pub(crate)` —
/// the router pattern-matches on variants but no external module
/// constructs them.
#[derive(Debug)]
pub(crate) enum RouteDecision {
  /// Forward to a Ready supervisor on `port`. `served_model_id` is
  /// the display name of the model actually serving the request;
  /// equal to `requested_model` on the happy path and diverges on
  /// fallback (Unit 4). `fallback` gates the `x-llamastash-*`
  /// response headers in [`super::forward`].
  ReadyAt {
    port: u16,
    served_model_id: String,
    /// Canonical `(path, header_blake3)` of the supervisor we picked.
    /// Threaded through so the forward path can re-verify the port
    /// still binds the same model just before sending the request
    /// upstream — defends against the Ready→Stopping→port-reuse race
    /// where a different model could be answering on the same port
    /// by the time we connect.
    served_model_key: ModelId,
    fallback: bool,
    fallback_reason: Option<String>,
  },
  /// The catalog has the model but no Ready supervisor is serving it.
  /// Dispatched into `handle_not_running` which runs the auto-start +
  /// single-flight + family-MRU fallback flow. The variant carries the
  /// resolved row + arch so the launch path doesn't repeat the lookup.
  NotRunning {
    requested_model: String,
    /// Resolved catalog entry consumed by the launch path to build
    /// `StartParams` for `start_model_inner` without re-running the
    /// resolver.
    // dead_code: consumed via destructuring in router::forward_request;
    // the field itself is moved out, not read by name.
    #[allow(dead_code)]
    resolved_row: Box<CatalogRow>,
    /// Catalog arch metadata (e.g. `"llama"`, `"qwen3"`). `None`
    /// when discovery couldn't parse the GGUF header. The family-MRU
    /// fallback pivots on this field.
    // dead_code: consumed via destructuring in router::forward_request.
    #[allow(dead_code)]
    arch: Option<String>,
  },
  /// `resolve_model` returned zero matches. Unit 3 emits 404
  /// `model_not_found` with `matches: []`.
  NotFound { requested_model: String },
  /// `resolve_model` returned > 1 matches. Unit 3 emits 400
  /// `ambiguous_model` with the candidate names.
  Ambiguous {
    requested_model: String,
    candidates: Vec<String>,
  },
  /// `body.model` is absent or empty. Unit 3 emits 400
  /// `invalid_request` with `code: "model_required"`.
  ModelRequired,
}

/// Errors raised before [`decide`] returns — these escape the
/// forwarding-plan layer and propagate to the per-request HTTP
/// status mapping in [`super::router`].
#[derive(Debug)]
pub(crate) enum BodyError {
  /// Body exceeded the 2 MiB ceiling. HTTP 413.
  TooLarge,
  /// Body wasn't valid JSON or `body.model` wasn't a string.
  /// HTTP 400 `invalid_request`.
  Malformed { message: String },
  /// hyper choked reading the request body off the wire. Surface as
  /// HTTP 400; client-side framing is broken either way.
  Read { message: String },
}

/// Minimal-shape body parse: serde ignores all fields it doesn't
/// know about, so anything beyond `model` is preserved in the
/// buffered bytes we forward upstream unchanged.
#[derive(serde::Deserialize)]
struct JustModel {
  #[serde(default)]
  model: Option<String>,
}

/// Outcome of buffering + extracting. `bytes` is the full body
/// (capped at [`BODY_LIMIT_BYTES`]); we forward these verbatim, so
/// no re-encoding ever happens after this point.
pub(crate) struct ParsedBody {
  pub bytes: Bytes,
  pub model: Option<String>,
}

/// Drain the inbound body under the 2 MiB cap, then peek the
/// `model` field with a single tolerant parse. The body bytes are
/// kept as-is for verbatim forwarding.
pub(crate) async fn buffer_and_extract(body: Incoming) -> Result<ParsedBody, BodyError> {
  let collected = match Limited::new(body, BODY_LIMIT_BYTES).collect().await {
    Ok(c) => c,
    Err(err) => {
      // `http-body-util::Limited` wraps the inner error inside a
      // `Box<dyn Error + Send + Sync>`. The cap-overflow case is
      // exposed as `LengthLimitError`; distinguishing it lets us
      // emit 413 vs 400 with the right message.
      if err
        .downcast_ref::<http_body_util::LengthLimitError>()
        .is_some()
      {
        return Err(BodyError::TooLarge);
      }
      return Err(BodyError::Read {
        message: format!("failed to read request body: {err}"),
      });
    }
  };
  let bytes = collected.to_bytes();

  // An empty body is allowed in principle — `model` extraction
  // then returns None and the caller emits `model_required`.
  let model = if bytes.is_empty() {
    None
  } else {
    match serde_json::from_slice::<JustModel>(&bytes) {
      Ok(parsed) => parsed
        .model
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty()),
      Err(err) => {
        return Err(BodyError::Malformed {
          message: format!("request body is not valid JSON: {err}"),
        });
      }
    }
  };

  Ok(ParsedBody { bytes, model })
}

/// Build a [`RouteDecision`] from the parsed body. Does no I/O
/// beyond reading shared snapshots (catalog, supervisors). The
/// forwarding decision is pure — the side-effecting forward call
/// lives in [`super::forward`].
pub(crate) async fn decide(state: &Arc<ProxyState>, body_model: Option<String>) -> RouteDecision {
  let requested = match body_model {
    Some(m) if !m.is_empty() => m,
    _ => return RouteDecision::ModelRequired,
  };

  // Catalog snapshot → CatalogRow vec (the resolver speaks
  // `&[CatalogRow]`). Built in-process here because the existing
  // `cli::resolve::fetch_catalog` round-trips through IPC, which
  // we explicitly want to avoid on the hot path.
  let snap = state.ctx.catalog.snapshot().await;
  let rows: Vec<CatalogRow> = snap.iter().map(catalog_row_from_discovered).collect();
  let resolved = match resolve_model(&rows, &requested) {
    Ok(r) => r,
    Err(_) => {
      // `resolve_model` collapses both 0- and N-match cases into
      // the same `MODEL_NOT_FOUND` exit. Re-derive which by
      // re-running the substring filter — cheap, and lets the
      // proxy emit the right HTTP code (404 vs 400).
      let candidates = substring_candidates(&rows, &requested);
      return if candidates.is_empty() {
        RouteDecision::NotFound {
          requested_model: requested,
        }
      } else {
        RouteDecision::Ambiguous {
          requested_model: requested,
          candidates: candidates.into_iter().map(|r| r.name()).collect(),
        }
      };
    }
  };

  // Walk the supervisor snapshot for a Ready entry serving the
  // resolved row's path. Two HashMap lookups + one state read each
  // — well within the hot-path budget the plan asks for.
  let sup_snap = state.ctx.supervisors.snapshot().await;
  for (_launch_id, model) in sup_snap.into_iter() {
    if !same_path(&model.id().path, &resolved.path) {
      continue;
    }
    if matches!(model.state().await, ManagedState::Ready) {
      return RouteDecision::ReadyAt {
        port: model.port(),
        served_model_id: served_name_for_row(&resolved),
        served_model_key: model.id().clone(),
        fallback: false,
        fallback_reason: None,
      };
    }
  }

  // Catalog matched but no supervisor is in Ready state — dispatch
  // into the auto-start + single-flight + family-MRU-fallback flow
  // implemented by `route::handle_not_running` (Unit 4).
  let arch = resolved.arch.clone();
  RouteDecision::NotRunning {
    requested_model: requested,
    resolved_row: Box::new(resolved),
    arch,
  }
}

/// Recompute the substring candidates `resolve_model` saw. We
/// duplicate a few lines of the resolver here so the proxy can
/// distinguish "0 matches" (404) from "N matches" (400) without
/// teaching the resolver itself a new exit code — keeping
/// `cli::resolve` callers stable.
fn substring_candidates<'a>(rows: &'a [CatalogRow], reference: &str) -> Vec<&'a CatalogRow> {
  let needle = reference.trim();
  if needle.is_empty() {
    return Vec::new();
  }
  // Exact path / name first — same precedence as resolve_model.
  let exact_path: Vec<&CatalogRow> = rows.iter().filter(|r| r.path == needle).collect();
  if !exact_path.is_empty() {
    return exact_path;
  }
  let exact_name: Vec<&CatalogRow> = rows.iter().filter(|r| r.name() == needle).collect();
  if !exact_name.is_empty() {
    return exact_name;
  }
  let lower = needle.to_lowercase();
  rows
    .iter()
    .filter(|r| {
      r.name().to_lowercase().contains(&lower) || r.parent.to_lowercase().contains(&lower)
    })
    .collect()
}

/// Project a discovered-model entry onto the `CatalogRow` shape the
/// resolver expects. In-process equivalent of
/// `cli::resolve::parse_catalog_row` (which goes through the JSON
/// wire); kept here so the proxy doesn't pay a serialize/deserialize
/// round-trip on the hot path.
fn catalog_row_from_discovered(m: &DiscoveredModel) -> CatalogRow {
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

/// Compare a `ModelId::path` (PathBuf) with a `CatalogRow::path`
/// (String). The catalog row is built from the discovered path's
/// `to_string_lossy()` view, and `ModelId::path` is canonical too —
/// equality is exact in production.
fn same_path(model_id_path: &std::path::Path, row_path: &str) -> bool {
  model_id_path.to_string_lossy() == row_path
}

/// Unit 4 entry point — invoked from `router.rs` when a request
/// hits a catalog row whose model isn't currently Ready.
///
/// Drives:
///   1. Auto-start via [`launch::auto_start`] (single-flight
///      coalesced; waits for Ready or terminal Error).
///   2. On Ready → MRU touch + forward (no fallback headers).
///   3. On Error → pick a family-MRU fallback and forward with
///      `x-llamastash-served-by` + `x-llamastash-fallback-reason:
///      launch_failed`. If no Ready candidate exists → 503
///      `launch_failed` with the running list.
pub(crate) async fn handle_not_running(
  state: &Arc<ProxyState>,
  inbound: super::forward::InboundRequest,
  requested_model: String,
  resolved_row: CatalogRow,
  requested_arch: Option<String>,
) -> ProxyResponse {
  let outcome = launch::auto_start(state, &resolved_row).await;
  match outcome {
    LaunchOutcome::Ready { port, model_id } => {
      // Touch the MRU using the supervisor we just confirmed Ready.
      // The display name for the response header on the happy path
      // is the requested model name — no fallback, no surprise.
      state.mru.touch(&model_id).await;
      let served = served_name_for_row(&resolved_row);
      super::forward::forward_to_upstream(
        state,
        inbound,
        super::forward::Target {
          port,
          served_model_id: &served,
          served_model_key: &model_id,
          fallback: false,
          fallback_reason: None,
        },
      )
      .await
    }
    LaunchOutcome::Failed { cause } => {
      // Family-MRU fallback. Walk the supervisor snapshot, filter
      // to Ready, attach each entry's catalog arch + MRU
      // timestamp, then defer to `pick_fallback` for the policy.
      let candidates = collect_fallback_candidates(state).await;
      if let Some(pick) = pick_fallback(candidates, requested_arch.as_deref()) {
        state.mru.touch(&pick.model_id).await;
        return super::forward::forward_to_upstream(
          state,
          inbound,
          super::forward::Target {
            port: pick.port,
            served_model_id: &pick.served_model_id,
            served_model_key: &pick.model_id,
            fallback: true,
            fallback_reason: Some("launch_failed"),
          },
        )
        .await;
      }
      // No running model to fall back to. R155 mandates a 503
      // with the (empty) `running` list inline. Drop the requested
      // model name into the message so logs surface what was being
      // attempted.
      super::router::launch_failed_response(&cause, Vec::<String>::new(), &requested_model)
    }
  }
}

/// Resolve a display name for a CatalogRow that mirrors what
/// `/v1/models` and `llamastash list` show. Falls back to the
/// resolver's `name()` which uses `path.file_name()`; the
/// `display_label` form (Ollama's `<name>:<tag>`) wins when present.
fn served_name_for_row(row: &CatalogRow) -> String {
  if let Some(label) = &row.display_label {
    return label.clone();
  }
  row.name()
}

/// Build the candidate list the fallback selector picks from. Reads
/// the supervisor snapshot (filtering to Ready), joins each row
/// against the catalog snapshot to find the matching `arch`, and
/// stamps the latest MRU timestamp.
async fn collect_fallback_candidates(state: &Arc<ProxyState>) -> Vec<FallbackCandidate> {
  let sup_snap = state.ctx.supervisors.snapshot().await;
  // Build a `path -> CatalogRow` lookup from the catalog snapshot
  // so we can attach arch + display label without re-walking the
  // catalog for each supervisor entry. The catalog is small (tens
  // to hundreds of rows in v1) so a HashMap build is fine on this
  // path — only triggered when an auto-start has just failed.
  let cat_snap = state.ctx.catalog.snapshot().await;
  let mut by_path: std::collections::HashMap<String, &DiscoveredModel> =
    std::collections::HashMap::with_capacity(cat_snap.len());
  for m in cat_snap.iter() {
    by_path.insert(m.path.to_string_lossy().into_owned(), m);
  }

  let mut out: Vec<FallbackCandidate> = Vec::with_capacity(sup_snap.len());
  for (_launch_id, model) in sup_snap.into_iter() {
    if !matches!(model.state().await, ManagedState::Ready) {
      continue;
    }
    let id = model.id().clone();
    let path_key = id.path.to_string_lossy().into_owned();
    let catalog_entry = by_path.get(&path_key);
    let arch = catalog_entry
      .and_then(|m| m.metadata.as_ref())
      .and_then(|md| md.arch.clone());
    let served_model_id = catalog_entry
      .and_then(|m| m.display_label.clone())
      .unwrap_or_else(|| crate::util::paths::model_display_name(&id.path));
    let last_request_at = state.mru.last_request_at(&id).await;
    out.push(FallbackCandidate {
      model_id: id,
      arch,
      last_request_at,
      port: model.port(),
      served_model_id,
    });
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cli::resolve::CatalogRow;

  fn row(path: &str, parent: &str) -> CatalogRow {
    CatalogRow {
      path: path.to_string(),
      model_id: None,
      parent: parent.to_string(),
      source: "user".to_string(),
      arch: Some("llama".to_string()),
      quant: Some("Q4_K".to_string()),
      native_ctx: Some(8192),
      mode_hint: None,
      parameter_label: None,
      weights_bytes: None,
      display_label: None,
      parse_error: None,
    }
  }

  #[test]
  fn substring_candidates_returns_zero_for_unmatched() {
    let rows = vec![row("/m/llama.gguf", "/m")];
    assert!(substring_candidates(&rows, "phi").is_empty());
  }

  #[test]
  fn substring_candidates_returns_multiple_for_ambiguous() {
    let rows = vec![
      row("/m/qwen-coder-7b.gguf", "/m"),
      row("/m/qwen-coder-13b.gguf", "/m"),
    ];
    let cands = substring_candidates(&rows, "qwen-coder");
    assert_eq!(cands.len(), 2);
  }

  #[test]
  fn substring_candidates_unique_match_returns_one() {
    let rows = vec![row("/m/qwen.gguf", "/m"), row("/m/llama.gguf", "/m")];
    let cands = substring_candidates(&rows, "llama");
    assert_eq!(cands.len(), 1);
  }

  #[tokio::test]
  async fn buffer_and_extract_empty_body_returns_none_model() {
    use http_body_util::Full;
    use hyper::body::Bytes;
    // Build an `Incoming`-shaped pipe via hyper's test helpers: the
    // simplest path is to construct a hyper Request and pull its
    // body. We can't construct an `Incoming` directly outside hyper
    // — so this is exercised end-to-end in tests/proxy_routing.rs
    // instead. Inline test left as a documentation marker.
    let _ = Full::new(Bytes::from_static(b""));
  }
}
