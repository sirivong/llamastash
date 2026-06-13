//! Resolve user-supplied model / launch references against the
//! daemon's catalog or active supervisor list.
//!
//! Model references (used by `start`, `presets`, `favorites`) accept:
//! - an absolute path (matched verbatim against the canonical path),
//! - a canonical model id (the BLAKE3 short fingerprint or full hex),
//! - a substring matched case-insensitively against the file name and
//!   the parent directory.
//!
//! Running-launch references (used by `stop`, `logs`) accept a
//! `LaunchId` (e.g. `L3`), a port number, or a case-insensitive
//! substring of the running model's file name / parent directory.
//! Multiple matches surface as `MODEL_NOT_FOUND` with a
//! disambiguation hint.

use anyhow::Result;
use serde_json::Value;

use crate::cli::exit_codes::{CliExit, MODEL_NOT_FOUND};
use crate::ipc::Client;

/// One row from `list_models`. Lean wrapper kept here so the resolver
/// can stay independent of the catalog's internal `DiscoveredModel`
/// shape.
#[derive(Debug, Clone)]
pub struct CatalogRow {
  /// Canonical absolute path to the launchable file (or shard 1).
  pub path: String,
  /// Short BLAKE3-derived canonical id (8 hex chars). Optional
  /// because the daemon's catalog computes it lazily — pre-launch
  /// rows omit it.
  pub model_id: Option<String>,
  pub parent: String,
  pub source: String,
  pub arch: Option<String>,
  pub quant: Option<String>,
  pub native_ctx: Option<u64>,
  pub mode_hint: Option<String>,
  pub parameter_label: Option<String>,
  /// GGUF weights footprint (sum of per-tensor storage bytes). `None`
  /// when the file is metadata-only or the header parse failed. Used
  /// by `list_human` for the SIZE column.
  pub weights_bytes: Option<u64>,
  /// Source-supplied human label preferred over the path's basename
  /// when set. Currently populated only for Ollama rows, where the
  /// content-addressed blob filename (`sha256-<hex>`) is hostile to
  /// scanning by eye.
  pub display_label: Option<String>,
  pub parse_error: Option<String>,
  /// Sibling shard paths for split GGUFs. Empty for single-file
  /// models. `path` is always shard 1; this carries shards 2..N so
  /// callers (`show`, future size aggregators) can compute the
  /// on-disk total without re-scanning the parent dir.
  pub split_siblings: Vec<String>,
  /// `true` when the GGUF header carried a `tokenizer.chat_template`
  /// string. Surfacing the boolean (not the full template) keeps the
  /// `list_models` wire shape lean; the template body is large.
  pub has_chat_template: bool,
  /// `true` when the GGUF carried a reasoning hint. Mirrors the
  /// `metadata.has_reasoning_hint` field on `list_models`.
  pub has_reasoning_hint: bool,
  /// `tokenizer.ggml.model` from the GGUF header (`"llama"`, `"qwen2"`).
  pub tokenizer_kind: Option<String>,
  /// `general.parameter_count` — the raw count behind
  /// `parameter_label` (`"7B"` is derived from `7e9`).
  pub total_parameters: Option<u64>,
}

impl CatalogRow {
  /// Friendly label for human matching and table rendering.
  /// `display_label` (Ollama's `<name>:<tag>`) wins when set; falls
  /// back to the path basename so non-Ollama rows render exactly as
  /// they did before R1.
  pub fn name(&self) -> String {
    if let Some(label) = &self.display_label {
      return label.clone();
    }
    std::path::Path::new(&self.path)
      .file_name()
      .map(|s| s.to_string_lossy().into_owned())
      .unwrap_or_else(|| self.path.clone())
  }
}

/// One row from `status`'s `models` array.
#[derive(Debug, Clone)]
pub struct RunningRow {
  pub launch_id: String,
  pub model_path: String,
  /// Full canonical `ModelId` object as emitted by the daemon (path
  /// + header_blake3). `None` if the wire shape omits it.
  pub id: Option<Value>,
  pub port: u16,
  pub mode: String,
  pub state: String,
  /// Failure cause from the daemon's `ManagedState::Error { cause }`
  /// payload. Surfaced so users (and agents) can see *why* a launch
  /// landed in the `error` state without having to scrape the log
  /// file separately. `None` for non-error states.
  pub state_cause: Option<String>,
  pub pid: Option<u64>,
  pub ready_at: Option<u64>,
  /// Per-launch params (ctx / reasoning / advanced / mode). Lets
  /// agents reproduce a launch without a separate `last_params`
  /// call.
  pub params: Option<Value>,
  /// Latest resident-set bytes for the supervised process. `None`
  /// before the per-PID sampler has primed (typically one tick after
  /// launch).
  pub latest_rss_bytes: Option<u64>,
  /// Latest CPU usage % (multi-core sum, so >100% is normal for
  /// inference workloads). `None` before the per-PID sampler primes.
  pub latest_cpu_pct: Option<f32>,
  /// Context window `--fit` actually chose, read from the child's
  /// `/props` on Ready (R6). `None` until the fetch lands, or when the
  /// build doesn't expose it. Carried so `status --json` surfaces the
  /// resolved window without re-querying the child.
  pub resolved_ctx: Option<u32>,
}

impl RunningRow {
  /// Human-friendly basename (mirrors [`CatalogRow::name`]). Strips
  /// the parent directory but keeps the extension so split-shard
  /// files stay distinguishable. Falls back to the raw path when it
  /// has no separator.
  pub fn name(&self) -> String {
    basename(&self.model_path)
  }
}

/// Internal basename helper shared by the row types above. Kept in
/// this module so the row impls stay self-contained — callers should
/// use `RunningRow::name()` / `ExternalRow::name()`, not this
/// function directly.
fn basename(path: &str) -> String {
  std::path::Path::new(path)
    .file_name()
    .map(|s| s.to_string_lossy().into_owned())
    .unwrap_or_else(|| path.to_string())
}

/// Fetch every catalog row via `list_models`. Centralised here so
/// resolvers and the `list` handler share parsing.
pub async fn fetch_catalog(client: &mut Client) -> Result<Vec<CatalogRow>, CliExit> {
  let body = client
    .call("list_models", None)
    .await
    .map_err(CliExit::from_client_error)?;
  let arr = body
    .get("models")
    .and_then(Value::as_array)
    .cloned()
    .unwrap_or_default();
  Ok(arr.into_iter().map(parse_catalog_row).collect())
}

fn parse_catalog_row(row: Value) -> CatalogRow {
  let path = row
    .get("path")
    .and_then(Value::as_str)
    .unwrap_or_default()
    .to_string();
  let parent = row
    .get("parent")
    .and_then(Value::as_str)
    .unwrap_or_default()
    .to_string();
  let source = row
    .get("source")
    .and_then(Value::as_str)
    .unwrap_or_default()
    .to_string();
  let metadata = row.get("metadata");
  let parse_error = row
    .get("parse_error")
    .and_then(Value::as_str)
    .map(str::to_string);
  let model_id = row
    .get("model_id")
    .and_then(Value::as_str)
    .map(str::to_string);
  CatalogRow {
    path,
    model_id,
    parent,
    source,
    arch: metadata
      .and_then(|m| m.get("arch"))
      .and_then(Value::as_str)
      .map(str::to_string),
    quant: metadata
      .and_then(|m| m.get("quant"))
      .and_then(Value::as_str)
      .map(str::to_string),
    native_ctx: metadata
      .and_then(|m| m.get("native_ctx"))
      .and_then(Value::as_u64),
    mode_hint: metadata
      .and_then(|m| m.get("mode_hint"))
      .and_then(Value::as_str)
      .map(str::to_string),
    parameter_label: metadata
      .and_then(|m| m.get("parameter_label"))
      .and_then(Value::as_str)
      .map(str::to_string),
    weights_bytes: metadata
      .and_then(|m| m.get("weights_bytes"))
      .and_then(Value::as_u64),
    display_label: row
      .get("display_label")
      .and_then(Value::as_str)
      .map(str::to_string),
    parse_error,
    split_siblings: row
      .get("split_siblings")
      .and_then(Value::as_array)
      .map(|arr| {
        arr
          .iter()
          .filter_map(|v| v.as_str().map(str::to_string))
          .collect()
      })
      .unwrap_or_default(),
    has_chat_template: metadata
      .and_then(|m| m.get("has_chat_template"))
      .and_then(Value::as_bool)
      .unwrap_or(false),
    has_reasoning_hint: metadata
      .and_then(|m| m.get("has_reasoning_hint"))
      .and_then(Value::as_bool)
      .unwrap_or(false),
    tokenizer_kind: metadata
      .and_then(|m| m.get("tokenizer_kind"))
      .and_then(Value::as_str)
      .map(str::to_string),
    total_parameters: metadata
      .and_then(|m| m.get("total_parameters"))
      .and_then(Value::as_u64),
  }
}

/// Find a catalog row that matches `reference`. Disambiguation rules
/// (in order):
/// 1. exact canonical-path match,
/// 2. exact name match (basename),
/// 3. case-insensitive substring of name OR parent dir.
///
/// Returns `MODEL_NOT_FOUND` when zero or many rows match. The error
/// message names every candidate when matches > 1 so callers can
/// re-issue with a tighter reference.
pub fn resolve_model(rows: &[CatalogRow], reference: &str) -> Result<CatalogRow, CliExit> {
  match resolve_model_with_candidates(rows, reference) {
    Ok(row) => Ok(row),
    Err(ResolveError::Empty) => Err(CliExit::new(
      MODEL_NOT_FOUND,
      "empty model reference; supply a name substring, absolute path, or short id",
    )),
    Err(ResolveError::None) => Err(CliExit::new(
      MODEL_NOT_FOUND,
      format!("no model matches `{reference}` ({} known)", rows.len()),
    )),
    Err(ResolveError::Many(candidates)) => {
      let names: Vec<String> = candidates.iter().map(|r| r.name()).collect();
      Err(CliExit::new(
        MODEL_NOT_FOUND,
        format!(
          "`{reference}` matches {} models: {}\nrefine the reference (full path or unique substring) and retry",
          candidates.len(),
          names.join(", ")
        ),
      ))
    }
  }
}

/// Variant of [`resolve_model`] that preserves the distinction between
/// "zero candidates" and "many candidates" so callers (the HTTP proxy
/// uses this to emit 404 vs 400 with `matches: [...]`) can branch
/// without re-running the substring matcher themselves.
///
/// Tiers + precedence are identical to [`resolve_model`]; the only
/// difference is that the multi-match error carries the candidate
/// list rather than a flattened error message.
pub fn resolve_model_with_candidates(
  rows: &[CatalogRow],
  reference: &str,
) -> Result<CatalogRow, ResolveError> {
  let needle = reference.trim();
  if needle.is_empty() {
    return Err(ResolveError::Empty);
  }

  // Tier 1: exact path / exact name. A full canonical path is
  // unambiguous by construction.
  let exact_path: Vec<&CatalogRow> = rows.iter().filter(|r| r.path == needle).collect();
  if exact_path.len() == 1 {
    return Ok(exact_path[0].clone());
  }
  let exact_name: Vec<&CatalogRow> = rows.iter().filter(|r| r.name() == needle).collect();
  if exact_name.len() == 1 {
    return Ok(exact_name[0].clone());
  }

  // Tier 2: case-insensitive substring of name OR parent.
  let lower = needle.to_lowercase();
  let candidates: Vec<&CatalogRow> = rows
    .iter()
    .filter(|r| {
      r.name().to_lowercase().contains(&lower) || r.parent.to_lowercase().contains(&lower)
    })
    .collect();
  match candidates.len() {
    0 => Err(ResolveError::None),
    1 => Ok(candidates[0].clone()),
    _ => Err(ResolveError::Many(
      candidates.into_iter().cloned().collect(),
    )),
  }
}

/// Distinguishes the three resolver failure modes the HTTP proxy
/// needs to surface as distinct HTTP responses (and which the CLI
/// folds together into a single `MODEL_NOT_FOUND` exit).
#[derive(Debug, Clone)]
pub enum ResolveError {
  /// Reference was empty after trimming.
  Empty,
  /// Zero candidates matched the reference. Proxy emits 404
  /// `model_not_found`.
  None,
  /// More than one candidate matched. Proxy emits 400
  /// `ambiguous_model` with the candidate list in `matches`.
  Many(Vec<CatalogRow>),
}

/// Index running rows by canonical model path. Returns the first
/// running row per path (multiple supervisors for one path is rare
/// but possible — the picker uses the most recent ready_at row first
/// when this matters; here the list view just needs *some* live row).
pub fn running_index(rows: &[RunningRow]) -> std::collections::HashMap<String, RunningRow> {
  let mut out = std::collections::HashMap::with_capacity(rows.len());
  for r in rows {
    out.entry(r.model_path.clone()).or_insert_with(|| r.clone());
  }
  out
}

/// Fetch the supervisor + external snapshot via `status`.
pub async fn fetch_status(client: &mut Client) -> Result<StatusSnapshot, CliExit> {
  let body = client
    .call("status", None)
    .await
    .map_err(CliExit::from_client_error)?;
  let models: Vec<RunningRow> = body
    .get("models")
    .and_then(Value::as_array)
    .map(|a| a.iter().filter_map(parse_running_row).collect())
    .unwrap_or_default();
  let external: Vec<ExternalRow> = body
    .get("external")
    .and_then(Value::as_array)
    .map(|a| a.iter().filter_map(parse_external_row).collect())
    .unwrap_or_default();
  let gpu = body.get("gpu").cloned().unwrap_or(Value::Null);
  // AGENTS.md: `host` is always an object on the wire; preserve it
  // verbatim so the CLI `status --json` mirrors the IPC contract.
  let host = body.get("host").cloned().unwrap_or(Value::Null);
  let daemon = body.get("daemon").and_then(parse_daemon_health);
  // Preserve the proxy block verbatim — the CLI `status --json` is
  // byte-shape-identical to the IPC wire format per the plan's
  // R161 contract. Older daemons that don't surface the field land
  // as `Value::Null` and the projection in `status_json` drops the
  // key entirely.
  let proxy = body.get("proxy").cloned().unwrap_or(Value::Null);
  // Backends block — verbatim copy of the daemon's `status.backends`
  // array (R3/R16). `Value::Null` when talking to a daemon that predates
  // the field; the formatter then skips the section.
  let backends = body.get("backends").cloned().unwrap_or(Value::Null);
  Ok(StatusSnapshot {
    models,
    external,
    gpu,
    host,
    daemon,
    proxy,
    backends,
  })
}

#[derive(Debug, Clone)]
pub struct StatusSnapshot {
  pub models: Vec<RunningRow>,
  pub external: Vec<ExternalRow>,
  pub gpu: Value,
  /// Host-level metrics (CPU%, RAM, GPU util/temp/VRAM aggregates,
  /// sampler backend). Always an object on the wire per
  /// `AGENTS.md::status IPC fields`; the CLI preserves it verbatim
  /// so `status --json` consumers see the same shape as raw IPC
  /// clients. `Value::Null` only when talking to a daemon that
  /// predates the field.
  pub host: Value,
  /// Daemon health preamble (`pid`, `uptime_seconds`,
  /// `active_connections`). Older daemons may omit the field, in
  /// which case this is `None` — the formatter silently skips it.
  pub daemon: Option<DaemonHealth>,
  /// Proxy listener block — `{enabled, listen, status, bind_error}`.
  /// Verbatim copy of the daemon's wire shape (Unit 5 / R161); the
  /// CLI `status --json` rewrites it byte-for-byte so agents that
  /// parse the IPC and the CLI see identical shapes. `Value::Null`
  /// when talking to a pre-Unit-5 daemon that omits the field.
  pub proxy: Value,
  /// Backends block — array of `{id, lifecycle, installed, accelerators}`
  /// (R3/R16). Verbatim copy of the daemon's wire shape; `Value::Null`
  /// against a daemon that predates the field.
  pub backends: Value,
}

#[derive(Debug, Clone)]
pub struct DaemonHealth {
  pub pid: u64,
  pub uptime_seconds: u64,
  pub active_connections: u64,
  /// Daemon build version (cargo pkg version at compile time).
  /// `None` when an older daemon omits the field.
  pub build: Option<String>,
  /// Path to the `llama-server` binary the daemon resolved at start.
  /// `None` when the daemon doesn't expose it or hasn't resolved one.
  pub server_path: Option<String>,
  /// HTTP control-plane URL the daemon bound on startup
  /// (e.g. `http://127.0.0.1:48134`). `None` when talking to a
  /// pre-Phase-A daemon that doesn't surface the field.
  pub ipc_url: Option<String>,
}

fn parse_daemon_health(v: &Value) -> Option<DaemonHealth> {
  let obj = v.as_object()?;
  Some(DaemonHealth {
    pid: obj.get("pid").and_then(Value::as_u64).unwrap_or(0),
    uptime_seconds: obj
      .get("uptime_seconds")
      .and_then(Value::as_u64)
      .unwrap_or(0),
    active_connections: obj
      .get("active_connections")
      .and_then(Value::as_u64)
      .unwrap_or(0),
    build: obj.get("build").and_then(Value::as_str).map(str::to_string),
    server_path: obj
      .get("server_path")
      .and_then(Value::as_str)
      .map(str::to_string),
    ipc_url: obj
      .get("ipc_url")
      .and_then(Value::as_str)
      .map(str::to_string),
  })
}

#[derive(Debug, Clone)]
pub struct ExternalRow {
  pub pid: u64,
  pub cmdline: String,
  pub model_path: Option<String>,
  /// Listening port parsed from the orphan's argv on the daemon
  /// side. `None` when the cmdline didn't carry `--port` / `-p`
  /// (rare for llamastash-launched orphans — the supervisor always
  /// emits the long flag). Surfaced into `status --json` so agents
  /// can diff against `ss`/`lsof` without re-parsing argv client-side.
  pub port: Option<u16>,
  /// True when the orphan's environment carried `LLAMASTASH_LAUNCHED=1`
  /// at sweep time — i.e. it was spawned by some llamastash
  /// instance (this daemon's previous run, a sibling UAT daemon,
  /// etc.). Drives `collect_in_use_ports` on the daemon side; here
  /// it lives so the `daemon status` formatter can flag the row.
  pub launched_by_llamastash: bool,
}

impl ExternalRow {
  /// Best-effort label for an external row. Prefers the discovered
  /// model path's basename so the row reads like a managed launch;
  /// falls back to the cmdline's basename when the path is unknown
  /// rather than dumping the full argv into a narrow column.
  pub fn name(&self) -> String {
    self
      .model_path
      .as_deref()
      .map(basename)
      .unwrap_or_else(|| basename(&self.cmdline))
  }
}

fn parse_running_row(v: &Value) -> Option<RunningRow> {
  let launch_id = v.get("launch_id")?.as_str()?.to_string();
  let id = v.get("id").cloned();
  // Path lives under id.path because the `status` handler nests the
  // ModelId; preserve the same shape so callers can show it.
  let model_path = v
    .get("id")
    .and_then(|model_id| model_id.get("path"))
    .and_then(Value::as_str)
    .unwrap_or_default()
    .to_string();
  let port = v.get("port")?.as_u64()? as u16;
  let mode = v
    .get("mode")
    .and_then(Value::as_str)
    .unwrap_or_default()
    .to_string();
  let state = parse_running_state_label(v)
    .map(str::to_string)
    .unwrap_or_default();
  let state_cause = v
    .get("state")
    .and_then(|s| s.get("cause"))
    .and_then(Value::as_str)
    .map(str::to_string);
  let pid = v.get("pid").and_then(Value::as_u64);
  let ready_at = v.get("ready_at").and_then(Value::as_u64);
  let params = v.get("params").cloned();
  let latest_rss_bytes = v.get("latest_rss_bytes").and_then(Value::as_u64);
  let latest_cpu_pct = v
    .get("latest_cpu_pct")
    .and_then(Value::as_f64)
    .map(|n| n as f32);
  let resolved_ctx = v
    .get("resolved_ctx")
    .and_then(Value::as_u64)
    .map(|n| n as u32);
  Some(RunningRow {
    launch_id,
    model_path,
    id,
    port,
    mode,
    state,
    state_cause,
    pid,
    ready_at,
    params,
    latest_rss_bytes,
    latest_cpu_pct,
    resolved_ctx,
  })
}

fn parse_running_state_label(v: &Value) -> Option<&str> {
  v.get("state").and_then(Value::as_str).or_else(|| {
    v.get("state")
      .and_then(|s| s.get("state"))
      .and_then(Value::as_str)
  })
}

fn parse_external_row(v: &Value) -> Option<ExternalRow> {
  let pid = v.get("pid")?.as_u64()?;
  let cmdline = v
    .get("cmdline")
    .and_then(Value::as_str)
    .unwrap_or_default()
    .to_string();
  let model_path = v
    .get("model_path")
    .and_then(Value::as_str)
    .map(str::to_string);
  let port = v
    .get("port")
    .and_then(Value::as_u64)
    .and_then(|p| u16::try_from(p).ok());
  let launched_by_llamastash = v
    .get("launched_by_llamastash")
    .and_then(Value::as_bool)
    .unwrap_or(false);
  Some(ExternalRow {
    pid,
    cmdline,
    model_path,
    port,
    launched_by_llamastash,
  })
}

/// Resolve a reference against the running snapshot. Tiers, first hit wins:
/// 1. numeric → port match;
/// 2. exact (case-insensitive) launch-id (`L<n>`);
/// 3. case-insensitive substring of the model file name or its parent dir —
///    the same reference shape `start` / `show` / `presets` / `favorites`
///    accept against the catalog, here matched against the running launches
///    (usage.md §Concepts "Model references"). So `logs gemma` / `stop qwen`
///    work, not just `logs L3` / `stop 41100`.
///
/// Multiple matches surface as `MODEL_NOT_FOUND` with the launch ids listed.
pub fn resolve_running(rows: &[RunningRow], reference: &str) -> Result<RunningRow, CliExit> {
  let needle = reference.trim();
  if needle.is_empty() {
    return Err(CliExit::new(
      MODEL_NOT_FOUND,
      "empty target; supply a launch id (e.g. L3), a port, or a model name",
    ));
  }
  if let Ok(port) = needle.parse::<u16>() {
    let by_port: Vec<&RunningRow> = rows.iter().filter(|r| r.port == port).collect();
    return single_or_error(by_port, reference);
  }
  // Case-insensitive launch-id match. The supervisor formats them as
  // `L<n>` so case is fixed in practice; lower-case both sides for
  // forgiveness.
  let lower = needle.to_lowercase();
  let by_id: Vec<&RunningRow> = rows
    .iter()
    .filter(|r| r.launch_id.to_lowercase() == lower)
    .collect();
  if !by_id.is_empty() {
    return single_or_error(by_id, reference);
  }
  // Fall back to a name / parent-dir substring against the running rows.
  let by_name: Vec<&RunningRow> = rows
    .iter()
    .filter(|r| {
      let path = std::path::Path::new(&r.model_path);
      let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
      let parent = path.parent().and_then(|p| p.to_str()).unwrap_or("");
      name.to_lowercase().contains(&lower) || parent.to_lowercase().contains(&lower)
    })
    .collect();
  single_or_error(by_name, reference)
}

/// [`resolve_running`], with a lazy catalog fallback for launches whose
/// user-facing name never appears in their path — an Ollama alias like
/// `gemma4:e2b` runs from a `sha256-…` blob, so the path-substring tier
/// can't see it. Only a zero-match miss falls through (ambiguity is
/// final); the fallback maps catalog display labels back to paths and
/// re-matches the running rows, costing one extra `list_models` call
/// only on that miss path.
pub async fn resolve_running_via_catalog(
  client: &mut Client,
  rows: &[RunningRow],
  reference: &str,
) -> Result<RunningRow, CliExit> {
  let miss = match resolve_running(rows, reference) {
    Ok(row) => return Ok(row),
    Err(e) => e,
  };
  // `single_or_error`'s zero-match message — ambiguous matches keep
  // their launch-id listing and never reach the catalog.
  if !miss
    .message
    .as_deref()
    .unwrap_or_default()
    .starts_with("no running launch matches")
  {
    return Err(miss);
  }
  let Ok(catalog) = fetch_catalog(client).await else {
    return Err(miss);
  };
  let lower = reference.trim().to_lowercase();
  let label_paths: Vec<String> = catalog
    .iter()
    .filter(|c| c.name().to_lowercase().contains(&lower))
    .map(|c| c.path.clone())
    .collect();
  let by_label: Vec<&RunningRow> = rows
    .iter()
    .filter(|r| label_paths.contains(&r.model_path))
    .collect();
  if by_label.is_empty() {
    return Err(miss);
  }
  single_or_error(by_label, reference)
}

fn single_or_error(matches: Vec<&RunningRow>, reference: &str) -> Result<RunningRow, CliExit> {
  match matches.len() {
    0 => Err(CliExit::new(
      MODEL_NOT_FOUND,
      format!("no running launch matches `{reference}`"),
    )),
    1 => Ok(matches[0].clone()),
    _ => {
      let ids: Vec<String> = matches.iter().map(|r| r.launch_id.clone()).collect();
      Err(CliExit::new(
        MODEL_NOT_FOUND,
        format!(
          "`{reference}` matches {} launches: {}",
          matches.len(),
          ids.join(", ")
        ),
      ))
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn row(path: &str, parent: &str) -> CatalogRow {
    CatalogRow {
      path: path.to_string(),
      model_id: None,
      parent: parent.to_string(),
      source: "user".to_string(),
      arch: Some("llama".to_string()),
      quant: Some("Q4_K".to_string()),
      native_ctx: Some(8192),
      mode_hint: Some("chat".to_string()),
      parameter_label: Some("7B".to_string()),
      weights_bytes: Some(4_200_000_000),
      display_label: None,
      parse_error: None,
      split_siblings: Vec::new(),
      has_chat_template: false,
      has_reasoning_hint: false,
      tokenizer_kind: None,
      total_parameters: None,
    }
  }

  #[test]
  fn exact_path_wins_even_with_overlap() {
    let rows = vec![
      row("/m/qwen-coder-7b.gguf", "/m"),
      row("/m/qwen-coder-13b.gguf", "/m"),
    ];
    let pick = resolve_model(&rows, "/m/qwen-coder-7b.gguf").unwrap();
    assert_eq!(pick.path, "/m/qwen-coder-7b.gguf");
  }

  #[test]
  fn substring_match_disambiguates_when_unique() {
    let rows = vec![row("/m/qwen-coder.gguf", "/m"), row("/m/llama.gguf", "/m")];
    let pick = resolve_model(&rows, "qwen").unwrap();
    assert!(pick.name().contains("qwen"));
  }

  #[test]
  fn ambiguous_substring_returns_disambiguation_hint() {
    let rows = vec![
      row("/m/qwen-coder-7b.gguf", "/m"),
      row("/m/qwen-coder-13b.gguf", "/m"),
    ];
    let err = resolve_model(&rows, "qwen-coder").unwrap_err();
    assert_eq!(err.code, MODEL_NOT_FOUND);
    let msg = err.to_string();
    assert!(msg.contains("qwen-coder-7b.gguf"), "got: {msg}");
    assert!(msg.contains("qwen-coder-13b.gguf"), "got: {msg}");
  }

  #[test]
  fn with_candidates_returns_many_for_ambiguous() {
    let rows = vec![
      row("/m/qwen-coder-7b.gguf", "/m"),
      row("/m/qwen-coder-13b.gguf", "/m"),
    ];
    match resolve_model_with_candidates(&rows, "qwen-coder") {
      Err(ResolveError::Many(cands)) => assert_eq!(cands.len(), 2),
      other => panic!("expected Many(2); got {other:?}"),
    }
  }

  #[test]
  fn with_candidates_returns_none_for_unmatched() {
    let rows = vec![row("/m/llama.gguf", "/m")];
    match resolve_model_with_candidates(&rows, "phi") {
      Err(ResolveError::None) => {}
      other => panic!("expected None; got {other:?}"),
    }
  }

  #[test]
  fn with_candidates_returns_empty_for_blank_reference() {
    match resolve_model_with_candidates(&[], "   ") {
      Err(ResolveError::Empty) => {}
      other => panic!("expected Empty; got {other:?}"),
    }
  }

  #[test]
  fn zero_matches_surfaces_not_found_with_count() {
    let rows = vec![row("/m/qwen-coder.gguf", "/m")];
    let err = resolve_model(&rows, "phi").unwrap_err();
    assert_eq!(err.code, MODEL_NOT_FOUND);
    assert!(err.to_string().contains("phi"));
  }

  #[test]
  fn empty_reference_errors_with_hint() {
    let err = resolve_model(&[], "  ").unwrap_err();
    assert_eq!(err.code, MODEL_NOT_FOUND);
    assert!(err.to_string().to_lowercase().contains("empty"));
  }

  #[test]
  fn parent_dir_substring_matches() {
    let rows = vec![
      row(
        "/cache/lm-studio/models/qwen.gguf",
        "/cache/lm-studio/models",
      ),
      row("/cache/ollama/models/llama.gguf", "/cache/ollama/models"),
    ];
    let pick = resolve_model(&rows, "lm-studio").unwrap();
    assert!(pick.parent.contains("lm-studio"));
  }

  #[test]
  fn resolve_running_by_port_matches() {
    let rows = vec![
      RunningRow {
        launch_id: "L1".into(),
        model_path: "/m/a.gguf".into(),
        id: None,
        port: 41100,
        mode: "chat".into(),
        state: "ready".into(),
        state_cause: None,
        pid: Some(123),
        ready_at: None,
        params: None,
        latest_rss_bytes: None,
        latest_cpu_pct: None,
        resolved_ctx: None,
      },
      RunningRow {
        launch_id: "L2".into(),
        model_path: "/m/b.gguf".into(),
        id: None,
        port: 41101,
        mode: "chat".into(),
        state: "ready".into(),
        state_cause: None,
        pid: Some(124),
        ready_at: None,
        params: None,
        latest_rss_bytes: None,
        latest_cpu_pct: None,
        resolved_ctx: None,
      },
    ];
    assert_eq!(resolve_running(&rows, "41100").unwrap().launch_id, "L1");
    assert_eq!(resolve_running(&rows, "L2").unwrap().launch_id, "L2");
  }

  #[test]
  fn resolve_running_unknown_port_errors() {
    let rows = vec![RunningRow {
      launch_id: "L1".into(),
      model_path: "/m/a.gguf".into(),
      id: None,
      port: 41100,
      mode: "chat".into(),
      state: "ready".into(),
      state_cause: None,
      pid: None,
      ready_at: None,
      params: None,
      latest_rss_bytes: None,
      latest_cpu_pct: None,
      resolved_ctx: None,
    }];
    let err = resolve_running(&rows, "9999").unwrap_err();
    assert_eq!(err.code, MODEL_NOT_FOUND);
  }

  #[test]
  fn resolve_running_by_name_substring() {
    let row = |id: &str, path: &str, port: u16| RunningRow {
      launch_id: id.into(),
      model_path: path.into(),
      id: None,
      port,
      mode: "chat".into(),
      state: "ready".into(),
      state_cause: None,
      pid: Some(1),
      ready_at: None,
      params: None,
      latest_rss_bytes: None,
      latest_cpu_pct: None,
      resolved_ctx: None,
    };
    let rows = vec![
      row("L1", "/cache/gemma-4-E2B-it-Q4_K_M.gguf", 41100),
      row("L2", "/cache/qwen3-reranker-0.6b-q8_0.gguf", 41101),
    ];
    // File-name substring — the documented model reference — resolves the
    // running launch (this is the F-04 regression: only L<n>/port worked).
    assert_eq!(resolve_running(&rows, "gemma").unwrap().launch_id, "L1");
    assert_eq!(
      resolve_running(&rows, "GEMMA-4-E2B-it-Q4_K_M.gguf")
        .unwrap()
        .launch_id,
      "L1"
    );
    assert_eq!(resolve_running(&rows, "reranker").unwrap().launch_id, "L2");
    // launch-id and port still take precedence and behave as before.
    assert_eq!(resolve_running(&rows, "L2").unwrap().launch_id, "L2");
    assert_eq!(resolve_running(&rows, "41100").unwrap().launch_id, "L1");
    // A substring matching multiple running launches (shared parent dir)
    // disambiguates rather than picking arbitrarily.
    assert_eq!(
      resolve_running(&rows, "cache").unwrap_err().code,
      MODEL_NOT_FOUND
    );
    // No match.
    assert_eq!(
      resolve_running(&rows, "nope").unwrap_err().code,
      MODEL_NOT_FOUND
    );
  }

  #[test]
  fn parse_running_row_accepts_nested_state_object() {
    let row = serde_json::json!({
      "launch_id": "L1",
      "id": {"path": "/m/a.gguf", "header_blake3": "deadbeef"},
      "port": 41100,
      "mode": "chat",
      "state": {"state": "ready"},
      "pid": 123,
      "ready_at": 456,
    });
    let parsed = parse_running_row(&row).expect("row should parse");
    assert_eq!(parsed.state, "ready");
  }

  #[test]
  fn parse_running_row_accepts_flat_state_string() {
    let row = serde_json::json!({
      "launch_id": "L1",
      "id": {"path": "/m/a.gguf", "header_blake3": "deadbeef"},
      "port": 41100,
      "mode": "chat",
      "state": "loading",
      "pid": 123,
      "ready_at": 456,
    });
    let parsed = parse_running_row(&row).expect("row should parse");
    assert_eq!(parsed.state, "loading");
  }
}
