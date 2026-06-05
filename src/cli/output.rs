//! Human + JSON output formatters shared by the non-interactive
//! subcommands.
//!
//! Two surfaces, one source of truth: every command supports `--json`
//! whose shape is the public agent contract; the human-readable form
//! is best-effort prettification.
//!
//! Tab-separated text is the default human format. Agents pin against
//! `--json`; humans get something `column -t` friendly.

use std::collections::HashMap;

use serde_json::Value;

use crate::cli::resolve::{CatalogRow, RunningRow, StatusSnapshot};
use crate::tui::status_icons::{glyph_for, SurfaceState};

/// Decode the canonical model path out of a daemon row's nested
/// `id.path` shape. Centralised so the five CLI subcommands that
/// project status / list_models / favorites / last_params rows
/// stop respelling the same two-level `get` (audit §1.1 #7).
pub fn row_path(v: &Value) -> Option<&str> {
  v.get("id")
    .and_then(|id| id.get("path"))
    .and_then(Value::as_str)
}

/// Render `list_models` rows as a padded table on TTY, or
/// tab-separated rows when colors are disabled (piped / `--no-colors` /
/// `NO_COLOR`). Columns: NAME, ARCH, QUANT, CTX, SIZE, STATUS.
///
/// SIZE displays the GGUF weights footprint (matches the TUI list
/// pane's SIZE column) — PATH was dropped because the canonical paths
/// dominated line width on real caches. `--json` still carries `path`
/// for agent consumers.
///
/// STATUS shows the live supervisor state when one exists for the
/// row's path: a TUI-shared glyph (`● ready`, `◐ loading`, …) followed
/// by `:<port>`. The column is empty for rows with no supervisor so
/// the catalog stays uncluttered. `running` is the index produced by
/// [`crate::cli::resolve::running_index`] — pass an empty map to opt out.
///
/// Footer line `(N models)` is appended on TTY only — the piped form
/// stays byte-stable for `awk -F\t` / `column -t` pipelines.
pub fn list_human(rows: &[CatalogRow], running: &HashMap<String, RunningRow>) -> String {
  use crate::cli::{colors, format};
  if rows.is_empty() {
    return format!("{}\n", colors::dim("(no models discovered)"));
  }
  let header = ["NAME", "ARCH", "QUANT", "CTX", "SIZE", "STATUS"];
  let body: Vec<Vec<String>> = rows
    .iter()
    .map(|r| {
      let arch = r.arch.as_deref().unwrap_or("?");
      let quant = r.quant.as_deref().unwrap_or("?");
      let ctx = r
        .native_ctx
        .map(|n| n.to_string())
        .unwrap_or_else(|| "?".to_string());
      // Compute the on-disk total via the shared shard-sizes util so
      // a row's SIZE always reflects shard 1 + every sibling shard,
      // independent of when the daemon last scanned (its cached
      // `weights_bytes` may predate a binary upgrade that fixed the
      // split-shard aggregation). One `stat` per row is cheap.
      let size = display_size(r);
      let status = running_status_cell(running.get(&r.path));
      vec![
        r.name(),
        arch.to_string(),
        quant.to_string(),
        ctx,
        size,
        status,
      ]
    })
    .collect();
  let mut out = format::table(&header, &body);
  if console::colors_enabled() {
    out.push_str(&colors::count(rows.len(), "models"));
    out.push('\n');
  }
  out
}

/// Render the STATUS cell. Empty for non-running rows; for running
/// rows: `<glyph> <state> :<port>`, reusing the TUI's `glyph_for`
/// mapping so the two surfaces never drift.
fn running_status_cell(row: Option<&RunningRow>) -> String {
  use crate::cli::colors;
  let Some(r) = row else {
    return String::new();
  };
  let surface = SurfaceState::from_wire_label(&r.state);
  let glyph = glyph_for(surface);
  let tty = console::colors_enabled();
  let state_label = if tty {
    colors::state(&r.state)
  } else {
    r.state.clone()
  };
  let port_part = format!(":{port}", port = r.port);
  let port_part = if tty {
    colors::dim(&port_part)
  } else {
    port_part
  };
  format!("{glyph} {state_label} {port_part}")
}

/// SIZE column for one row. Tries the shared shard-sizes util first
/// (sums shard 1 + every sibling on disk); falls back to the wire
/// shape's `weights_bytes` when neither path exists yet (a row that
/// surfaced from the catalog but was deleted between scan and
/// render), and finally to `?` when even that is absent.
pub(crate) fn display_size(row: &CatalogRow) -> String {
  use std::path::PathBuf;
  let primary = PathBuf::from(&row.path);
  let siblings: Vec<PathBuf> = row.split_siblings.iter().map(PathBuf::from).collect();
  let total = crate::discovery::shard_sizes::on_disk_total(&primary, &siblings);
  if total > 0 {
    return crate::tui::fmt::format_bytes(total);
  }
  row
    .weights_bytes
    .map(crate::tui::fmt::format_bytes)
    .unwrap_or_else(|| "?".to_string())
}

/// JSON projection of `list_models` rows. Stable shape — agents pin
/// against this, so column drift requires deliberate intent. Wrapped
/// in `{"models": [...]}` so every CLI `--json` surface lives behind
/// the same "always object at the root" rule.
pub fn list_json(rows: &[CatalogRow], running: &HashMap<String, RunningRow>) -> Value {
  let arr: Vec<Value> = rows
    .iter()
    .map(|r| {
      // Emit `model_id` only when populated. The IPC `list_models`
      // doesn't currently include it (the catalog has no scan-time
      // BLAKE3 yet), so leaving the field present as `null` would
      // mislead agents into thinking a stable handle exists.
      let mut row = serde_json::json!({
        "name": r.name(),
        "path": r.path,
        "parent": r.parent,
        "source": r.source,
        "arch": r.arch,
        "quant": r.quant,
        "native_ctx": r.native_ctx,
        "mode_hint": r.mode_hint,
        "parameter_label": r.parameter_label,
        "weights_bytes": r.weights_bytes,
        "display_label": r.display_label,
        "parse_error": r.parse_error,
      });
      if let Some(id) = &r.model_id {
        row["model_id"] = serde_json::Value::String(id.clone());
      }
      // `status` is a small nested object so agents can pin
      // `models[i].status.state` / `.port` rather than two flat
      // `status_state` / `status_port` keys. Absent (not `null`) when
      // the model has no live supervisor.
      if let Some(live) = running.get(&r.path) {
        row["status"] = serde_json::json!({
          "state": live.state,
          "port": live.port,
          "launch_id": live.launch_id,
        });
      }
      row
    })
    .collect();
  serde_json::json!({"models": arr})
}

/// JSON projection of `favorite_list` rows. Wrapped in
/// `{"favorites": [...]}` (matches the rest of the CLI surface).
/// Each row carries `path` + `name` at the root for symmetry with
/// `list_json` so consumers don't need to descend two levels via
/// `id.path`.
pub fn favorites_json(rows: &[Value]) -> Value {
  let arr: Vec<Value> = rows
    .iter()
    .map(|r| {
      let path = row_path(r).unwrap_or("");
      let name = std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
      serde_json::json!({
        "name": name,
        "path": path,
        "id": r.get("id").cloned().unwrap_or(Value::Null),
      })
    })
    .collect();
  serde_json::json!({"favorites": arr})
}

/// Filter catalog rows by case-insensitive substring against name,
/// path, arch, and quant. Matches the `list --filter` semantics
/// documented in the plan.
pub fn filter_rows(rows: &[CatalogRow], pattern: &str) -> Vec<CatalogRow> {
  let lower = pattern.to_lowercase();
  rows
    .iter()
    .filter(|r| {
      r.name().to_lowercase().contains(&lower)
        || r.path.to_lowercase().contains(&lower)
        || r
          .arch
          .as_deref()
          .map(|a| a.to_lowercase().contains(&lower))
          .unwrap_or(false)
        || r
          .quant
          .as_deref()
          .map(|a| a.to_lowercase().contains(&lower))
          .unwrap_or(false)
    })
    .cloned()
    .collect()
}

/// Human rendering of `status`.
///
/// Two surfaces share one source:
/// - On TTY (`colors_enabled()`): section header for the daemon, a
///   `kv_block` for its fields, a padded launches table, and a dim
///   GPU footer line.
/// - On non-TTY / `--no-colors`: today's single-line daemon preamble
///   plus byte-stable TSV rows — preserves `awk -F\t` / `column -t`
///   pipelines.
///
/// Columns are stable across modes (LAUNCH_ID, STATE, MODE, PORT, PID,
/// NAME). NAME is the file basename so narrow terminals don't get
/// crushed by canonical paths; `--json` keeps the full `model_path`
/// for agents. RSS/CPU% are intentionally not surfaced here even when
/// the per-PID sampler has primed them — they belong in a future
/// `--detail` view rather than always-on columns.
pub fn status_human(snap: &StatusSnapshot) -> String {
  use crate::cli::{colors, format};

  let tty = console::colors_enabled();
  let mut out = String::new();

  // Daemon preamble.
  if let Some(d) = &snap.daemon {
    if tty {
      out.push_str(&format::section_header("daemon", None));
      let pid_styled = console::style(d.pid.to_string()).bold().to_string();
      let uptime = format::format_uptime(d.uptime_seconds);
      let mut rows: Vec<(&'static str, String)> = vec![
        ("pid", pid_styled),
        ("uptime", uptime),
        ("connections", d.active_connections.to_string()),
      ];
      // Proxy row — surfaced inline in the daemon block when the
      // proxy is enabled. Skipped (per plan §Test scenarios edge
      // case 3) when disabled. The label cycles through the same
      // wire labels the IPC emits so a user grepping `status` text
      // matches the same strings agents key on.
      if let Some(line) = proxy_human_label(&snap.proxy) {
        rows.push(("proxy", line));
      }
      out.push_str(&format::kv_block(&rows));
      out.push('\n');
    } else {
      out.push_str(&format!(
        "daemon: pid={} uptime={}s connections={}\n",
        d.pid, d.uptime_seconds, d.active_connections,
      ));
      if let Some(line) = proxy_human_label(&snap.proxy) {
        out.push_str(&format!("proxy: {line}\n"));
      }
    }
  } else if let Some(line) = proxy_human_label(&snap.proxy) {
    // No daemon block (older daemon) but the proxy field is
    // present — surface it on its own line so the user can still
    // see the listener state.
    if tty {
      out.push_str(&format::section_header("proxy", None));
      out.push_str(&format::kv_block(&[("status", line)]));
      out.push('\n');
    } else {
      out.push_str(&format!("proxy: {line}\n"));
    }
  }

  // Launches table.
  if snap.models.is_empty() && snap.external.is_empty() {
    out.push_str(&colors::dim("(no managed launches)"));
    out.push('\n');
  } else {
    let header = ["LAUNCH_ID", "STATE", "MODE", "PORT", "PID", "NAME"];
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(snap.models.len() + snap.external.len());
    for r in &snap.models {
      let pid = r
        .pid
        .map(|p| p.to_string())
        .unwrap_or_else(|| "-".to_string());
      let name = r.name();
      rows.push(vec![
        if tty {
          colors::launch_id(&r.launch_id)
        } else {
          r.launch_id.clone()
        },
        if tty {
          colors::state(&r.state)
        } else {
          r.state.clone()
        },
        r.mode.clone(),
        if tty {
          colors::port(r.port)
        } else {
          r.port.to_string()
        },
        pid,
        name,
      ]);
    }
    for r in &snap.external {
      let name = r.name();
      // External rows are styled dim end-to-end so they read as
      // observer-only entries vs the bright managed ones.
      let dim_or_plain = |s: &str| if tty { colors::dim(s) } else { s.to_string() };
      rows.push(vec![
        dim_or_plain("external"),
        dim_or_plain("external"),
        dim_or_plain("-"),
        dim_or_plain("-"),
        dim_or_plain(&r.pid.to_string()),
        if tty { colors::dim(&name) } else { name },
      ]);
    }
    out.push_str(&format::table(&header, &rows));
    // Surface the failure cause for any error-state row beneath the
    // table — long-form, so a `health probe timeout` message and its
    // stderr tail don't trash the column widths. Without this the
    // user has to scrape the log file just to see *why* a launch
    // ended up in `error`.
    let causes: Vec<(&str, &str)> = snap
      .models
      .iter()
      .filter_map(|r| Some((r.launch_id.as_str(), r.state_cause.as_deref()?)))
      .collect();
    if !causes.is_empty() {
      out.push('\n');
      for (lid, cause) in causes {
        let cause_header = if tty {
          colors::dim(&format!("{lid} cause:"))
        } else {
          format!("{lid} cause:")
        };
        out.push_str(&format!("{cause_header} {cause}\n"));
      }
    }
  }

  // GPU footer.
  if let Some(label) = gpu_label(&snap.gpu) {
    out.push('\n');
    if tty {
      out.push_str(&colors::dim(&format!("GPU: {label}")));
      out.push('\n');
    } else {
      out.push_str(&format!("GPU: {label}\n"));
    }
  }
  out
}

fn gpu_label(gpu: &Value) -> Option<String> {
  // GpuInfo serialises as `{"backend": "<name>", ...}` — see
  // `gpu::GpuInfo`'s `#[serde(tag = "backend", rename_all = "snake_case")]`
  // attribute. Earlier versions of this function pattern-matched on
  // PascalCase variant keys (`Nvidia`, `Amd`, `Metal`, `Vulkan`)
  // which the current wire shape never emits, so every non-CpuOnly
  // backend silently fell through to the JSON-blob branch. Match on
  // the tagged-enum shape instead.
  use crate::daemon::host_metrics::GpuFlavor;
  if gpu.is_null() {
    return None;
  }
  let obj = gpu.as_object()?;
  let raw = obj.get("backend").and_then(Value::as_str)?;
  let count = || {
    obj
      .get("devices")
      .and_then(Value::as_array)
      .map(|a| a.len())
      .unwrap_or(0)
  };
  match GpuFlavor::from_label(raw) {
    GpuFlavor::CpuOnly => Some("CPU only".to_string()),
    GpuFlavor::Nvidia => Some(format!("NVIDIA GPU(s): {}", count())),
    GpuFlavor::Amd => Some(format!("AMD GPU(s): {}", count())),
    GpuFlavor::AppleMetal => {
      let total = obj
        .get("total_memory_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(0);
      let gib = total as f64 / (1024.0 * 1024.0 * 1024.0);
      Some(format!("Apple Silicon: {gib:.0}G unified"))
    }
    GpuFlavor::Unknown => Some(format!(
      "Unknown GPU vendor (Vulkan-only): {} device(s)",
      count()
    )),
    GpuFlavor::Multi => {
      let devs = obj
        .get("gpu_devices")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(count());
      // Count per-backend by examining selector prefixes
      let nvidia = obj
        .get("gpu_devices")
        .and_then(Value::as_array)
        .map(|a| {
          a.iter()
            .filter(|v| {
              v.get("selector")
                .and_then(Value::as_str)
                .map(|s| s.starts_with('N'))
                .unwrap_or(false)
            })
            .count()
        })
        .unwrap_or(0);
      let amd = obj
        .get("gpu_devices")
        .and_then(Value::as_array)
        .map(|a| {
          a.iter()
            .filter(|v| {
              v.get("selector")
                .and_then(Value::as_str)
                .map(|s| s.starts_with('A'))
                .unwrap_or(false)
            })
            .count()
        })
        .unwrap_or(0);
      let other = devs.saturating_sub(nvidia).saturating_sub(amd);
      let parts: Vec<String> = vec![
        if nvidia > 0 {
          Some(format!("NVIDIA GPU(s): {}", nvidia))
        } else {
          None
        },
        if amd > 0 {
          Some(format!("AMD GPU(s): {}", amd))
        } else {
          None
        },
        if other > 0 {
          Some(format!("Unknown GPU(s): {}", other))
        } else {
          None
        },
      ]
      .into_iter()
      .flatten()
      .collect();
      Some(parts.join(" + "))
    }
    GpuFlavor::Unsampled => Some(serde_json::to_string(gpu).unwrap_or_else(|_| "?".to_string())),
  }
}

/// Format the proxy block for the human-readable status table.
/// Returns `None` when the proxy is disabled or the block is absent
/// — the row is then skipped entirely so a config that turns the
/// proxy off doesn't add noise to the table (plan §Test scenarios
/// edge case 3).
///
/// Examples:
/// - listening  → `listening 127.0.0.1:11434`
/// - port_in_use → `port_in_use 127.0.0.1:11434 (port taken)`
/// - unbound    → `unbound 127.0.0.1:80 (permission denied)`
fn proxy_human_label(proxy: &Value) -> Option<String> {
  let obj = proxy.as_object()?;
  let status = obj.get("status").and_then(Value::as_str)?;
  if status == "disabled" {
    return None;
  }
  let listen = obj.get("listen").and_then(Value::as_str).unwrap_or("?");
  match status {
    "listening" => Some(format!("listening {listen}")),
    "port_in_use" => Some(format!("port_in_use {listen} (port taken)")),
    "unbound" => {
      let cause = obj
        .get("bind_error")
        .and_then(Value::as_str)
        .unwrap_or("bind failed");
      Some(format!("unbound {listen} ({cause})"))
    }
    other => Some(format!("{other} {listen}")),
  }
}

/// JSON projection of `status` (preserves the daemon's wire shape so
/// agents that already parse `daemon status` keep working).
///
/// Per the api-contract review:
/// * Each row preserves the daemon's `id` object (`{path,
///   header_blake3}`) so consumers can key by the canonical
///   fingerprint, not just the path string. `model_path` survives as
///   a convenience alongside.
/// * Each row also carries `params` (ctx / port / reasoning / mode /
///   advanced) so an agent can answer "how was this launch
///   configured" without a separate `last_params` round-trip.
/// * External rows synthesise `launch_id: "ext-<pid>"` to match the
///   TUI's identifier shape, so `stop ext-<pid>` lines up across
///   surfaces.
pub fn status_json(snap: &StatusSnapshot) -> Value {
  let models: Vec<Value> = snap
    .models
    .iter()
    .map(|r| {
      let mut obj = serde_json::Map::new();
      obj.insert("launch_id".into(), serde_json::json!(r.launch_id));
      // Preserve full `id` object alongside the flat path.
      if let Some(id) = r.id.as_ref() {
        obj.insert("id".into(), id.clone());
      }
      obj.insert("model_path".into(), serde_json::json!(r.model_path));
      obj.insert("port".into(), serde_json::json!(r.port));
      obj.insert("mode".into(), serde_json::json!(r.mode));
      obj.insert("state".into(), serde_json::json!(r.state));
      if let Some(cause) = r.state_cause.as_deref() {
        obj.insert("state_cause".into(), serde_json::json!(cause));
      }
      obj.insert("pid".into(), serde_json::json!(r.pid));
      obj.insert("ready_at".into(), serde_json::json!(r.ready_at));
      if let Some(params) = r.params.as_ref() {
        obj.insert("params".into(), params.clone());
      }
      // Per-PID resource snapshot from the supervisor sampler. `None`
      // before the sampler primes (one tick after launch).
      obj.insert(
        "latest_rss_bytes".into(),
        serde_json::json!(r.latest_rss_bytes),
      );
      obj.insert("latest_cpu_pct".into(), serde_json::json!(r.latest_cpu_pct));
      Value::Object(obj)
    })
    .collect();
  let external: Vec<Value> = snap
    .external
    .iter()
    .map(|r| {
      serde_json::json!({
        "launch_id": format!("ext-{}", r.pid),
        "pid": r.pid,
        "cmdline": r.cmdline,
        "model_path": r.model_path,
        "port": r.port,
        "launched_by_llamastash": r.launched_by_llamastash,
      })
    })
    .collect();
  let daemon = snap.daemon.as_ref().map(|d| {
    serde_json::json!({
      "pid": d.pid,
      "uptime_seconds": d.uptime_seconds,
      "active_connections": d.active_connections,
      "build": d.build,
      "server_path": d.server_path,
      "ipc_url": d.ipc_url,
    })
  });
  let mut body = serde_json::json!({
    "models": models,
    "external": external,
    "gpu": snap.gpu,
    "host": snap.host,
    "daemon": daemon,
  });
  // Proxy block — surfaced byte-for-byte from the IPC `status`
  // response so agents that parse `status --json` see the same
  // shape as raw IPC clients (R161). Pre-Unit-5 daemons emit no
  // block; we mirror that by omitting the key entirely rather than
  // synthesising a placeholder.
  if !snap.proxy.is_null() {
    if let Some(obj) = body.as_object_mut() {
      obj.insert("proxy".into(), snap.proxy.clone());
    }
  }
  body
}

/// Pretty-print `serde_json::Value` as the canonical CLI JSON form.
/// Agents pin against the pretty form because it's diffable in CI;
/// keep this consistent across every `--json` exit.
pub fn pretty_json(v: &Value) -> String {
  serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cli::resolve::{ExternalRow, RunningRow};
  use crate::cli::test_lock::serialize;
  use std::sync::MutexGuard;

  struct ColorGuard {
    _lock: MutexGuard<'static, ()>,
    prior: bool,
  }

  impl ColorGuard {
    fn set(enabled: bool) -> Self {
      let g = Self {
        _lock: serialize(),
        prior: console::colors_enabled(),
      };
      console::set_colors_enabled(enabled);
      g
    }
  }

  impl Drop for ColorGuard {
    fn drop(&mut self) {
      console::set_colors_enabled(self.prior);
    }
  }

  fn row(name: &str, arch: &str, quant: &str, ctx: u64) -> CatalogRow {
    CatalogRow {
      path: format!("/m/{name}.gguf"),
      model_id: Some(format!("{name:.8}")),
      parent: "/m".to_string(),
      source: "user".to_string(),
      arch: Some(arch.to_string()),
      quant: Some(quant.to_string()),
      native_ctx: Some(ctx),
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
  fn list_human_tsv_branch_emits_byte_exact_today_shape() {
    // Regression guard: piped consumers see exactly today's TSV bytes.
    // Snapshot string is the exact format the pre-padded-table code
    // produced so awk/cut/column pipelines don't drift.
    let _g = ColorGuard::set(false);
    let rows = vec![row("qwen", "qwen2", "Q4_K", 8192)];
    let s = list_human(&rows, &HashMap::new());
    assert_eq!(
      s,
      "NAME\tARCH\tQUANT\tCTX\tSIZE\tSTATUS\nqwen.gguf\tqwen2\tQ4_K\t8192\t3.9G\t\n"
    );
  }

  #[test]
  fn list_human_tty_branch_pads_columns_and_appends_count_footer() {
    let _g = ColorGuard::set(true);
    let rows = vec![
      row("qwen", "qwen2", "Q4_K", 8192),
      row("phi", "phi3", "Q5_K", 4096),
    ];
    let s = list_human(&rows, &HashMap::new());
    let plain = console::strip_ansi_codes(&s);
    assert!(plain.starts_with("NAME"), "header missing: {plain:?}");
    assert!(
      !plain.contains("NAME\t"),
      "padded output must not contain tabs in header: {plain:?}"
    );
    assert!(plain.contains("qwen.gguf"));
    assert!(plain.contains("(2 models)"), "footer missing: {plain:?}");
  }

  #[test]
  fn list_human_handles_empty_catalog() {
    // Same dim line in both modes — empty-state message is plain
    // bytes either way.
    for enabled in [true, false] {
      let _g = ColorGuard::set(enabled);
      let s = list_human(&[], &HashMap::new());
      assert!(console::strip_ansi_codes(&s).contains("no models"));
    }
  }

  #[test]
  fn list_json_wraps_rows_in_models_object_with_documented_keys() {
    let rows = vec![row("qwen", "qwen2", "Q4_K", 8192)];
    let v = list_json(&rows, &HashMap::new());
    let arr = v
      .get("models")
      .and_then(|m| m.as_array())
      .expect("models array");
    assert_eq!(arr.len(), 1);
    let r = &arr[0];
    for key in [
      "name",
      "path",
      "model_id",
      "parent",
      "source",
      "arch",
      "quant",
      "native_ctx",
      "mode_hint",
      "parameter_label",
      "parse_error",
    ] {
      assert!(r.get(key).is_some(), "key `{key}` missing in JSON row");
    }
  }

  #[test]
  fn list_json_empty_catalog_returns_empty_models_array() {
    let v = list_json(&[], &HashMap::new());
    assert_eq!(v, serde_json::json!({"models": []}));
  }

  #[test]
  fn list_json_omits_model_id_when_none() {
    // The IPC `list_models` response doesn't currently include
    // `model_id`. `list_json` must drop the field entirely rather
    // than serialise it as `null`, so agents that test
    // `row.model_id != null` don't have to special-case a missing
    // BLAKE3 column.
    let mut r = row("qwen", "qwen2", "Q4_K", 8192);
    r.model_id = None;
    let v = list_json(&[r], &HashMap::new());
    let row_v = v["models"][0].clone();
    assert!(
      row_v.get("model_id").is_none(),
      "model_id must be absent (not null) when CatalogRow.model_id is None"
    );
  }

  #[test]
  fn filter_rows_matches_name_arch_quant() {
    let rows = vec![
      row("qwen", "qwen2", "Q4_K", 8192),
      row("phi", "phi3", "Q5_K", 4096),
    ];
    assert_eq!(filter_rows(&rows, "qwen").len(), 1);
    assert_eq!(filter_rows(&rows, "Q5").len(), 1);
    assert_eq!(filter_rows(&rows, "phi3").len(), 1);
    assert_eq!(filter_rows(&rows, "missing").len(), 0);
  }

  #[test]
  fn status_human_handles_empty_snapshot() {
    // Both modes produce the same "no managed launches" dim line.
    for enabled in [true, false] {
      let _g = ColorGuard::set(enabled);
      let snap = StatusSnapshot {
        models: vec![],
        external: vec![],
        gpu: Value::Null,
        host: Value::Null,
        daemon: None,
        proxy: Value::Null,
      };
      let s = status_human(&snap);
      assert!(console::strip_ansi_codes(&s).contains("no managed"));
    }
  }

  #[test]
  fn status_human_includes_gpu_label_when_present() {
    // The live wire shape is `{"backend": "cpu_only"}` (snake_case
    // tagged enum); the test feeds the same shape the daemon emits,
    // not the legacy PascalCase variant key the function used to
    // match against. The label content is the same in both modes;
    // only the surrounding color styling differs.
    for enabled in [true, false] {
      let _g = ColorGuard::set(enabled);
      let snap = StatusSnapshot {
        models: vec![],
        external: vec![],
        gpu: serde_json::json!({"backend": "cpu_only"}),
        host: Value::Null,
        daemon: None,
        proxy: Value::Null,
      };
      let s = status_human(&snap);
      let plain = console::strip_ansi_codes(&s);
      assert!(plain.contains("CPU only"), "got: {plain}");
    }
  }

  #[test]
  fn gpu_label_matches_tagged_enum_shape_for_each_backend() {
    // The daemon serialises GpuInfo with `tag = "backend",
    // rename_all = "snake_case"`. Each backend must produce a
    // human-readable label rather than falling through to the JSON
    // blob branch.
    let nv = serde_json::json!({
      "backend": "nvidia",
      "devices": [
        {"name": "RTX 4090", "total_memory_bytes": 24, "used_memory_bytes": 0},
      ],
    });
    assert_eq!(gpu_label(&nv).as_deref(), Some("NVIDIA GPU(s): 1"));
    let amd = serde_json::json!({
      "backend": "amd",
      "devices": [
        {"name": "RX 7900", "total_memory_bytes": 24, "used_memory_bytes": 0},
        {"name": "RX 7800", "total_memory_bytes": 16, "used_memory_bytes": 0},
      ],
    });
    assert_eq!(gpu_label(&amd).as_deref(), Some("AMD GPU(s): 2"));
    let metal = serde_json::json!({
      "backend": "apple_metal",
      "total_memory_bytes": 64u64 * 1024 * 1024 * 1024,
    });
    assert_eq!(
      gpu_label(&metal).as_deref(),
      Some("Apple Silicon: 64G unified")
    );
    let unknown = serde_json::json!({
      "backend": "unknown",
      "devices": [{"name": "Vulkan device", "total_memory_bytes": 0, "used_memory_bytes": 0}],
    });
    assert_eq!(
      gpu_label(&unknown).as_deref(),
      Some("Unknown GPU vendor (Vulkan-only): 1 device(s)")
    );
  }

  #[test]
  fn status_human_tsv_branch_emits_legacy_daemon_preamble_byte_shape() {
    // Piped consumers parsing today's `pid=N` / `connections=N` form
    // stay supported byte-for-byte.
    let _g = ColorGuard::set(false);
    use crate::cli::resolve::DaemonHealth;
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: Some(DaemonHealth {
        pid: 4242,
        uptime_seconds: 90,
        active_connections: 3,
        build: None,
        server_path: None,
        ipc_url: None,
      }),
      proxy: Value::Null,
    };
    let s = status_human(&snap);
    assert!(s.starts_with("daemon: pid=4242"), "preamble shape: {s:?}");
    assert!(s.contains("uptime=90s"));
    assert!(s.contains("connections=3"));
  }

  #[test]
  fn status_human_tty_branch_renders_daemon_section_header_and_kv_block() {
    let _g = ColorGuard::set(true);
    use crate::cli::resolve::DaemonHealth;
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: Some(DaemonHealth {
        pid: 4242,
        uptime_seconds: 90,
        active_connections: 3,
        build: None,
        server_path: None,
        ipc_url: None,
      }),
      proxy: Value::Null,
    };
    let s = status_human(&snap);
    let plain = console::strip_ansi_codes(&s);
    assert!(plain.starts_with("daemon\n"), "section header: {plain:?}");
    assert!(plain.contains("pid  4242"), "kv pid: {plain:?}");
    assert!(plain.contains("uptime  1m 30s"), "kv uptime: {plain:?}");
    assert!(
      plain.contains("connections  3"),
      "kv connections: {plain:?}"
    );
  }

  #[test]
  fn status_json_round_trips_documented_keys() {
    let snap = StatusSnapshot {
      models: vec![RunningRow {
        launch_id: "L1".into(),
        model_path: "/m/a.gguf".into(),
        id: Some(serde_json::json!({"path": "/m/a.gguf", "header_blake3": "deadbeef"})),
        port: 41100,
        mode: "chat".into(),
        state: "ready".into(),
        state_cause: None,
        pid: Some(123),
        ready_at: Some(1_700_000_000),
        params: None,
        latest_rss_bytes: Some(4_500_000_000),
        latest_cpu_pct: Some(312.0),
      }],
      external: vec![ExternalRow {
        pid: 999,
        cmdline: "llama-server".into(),
        model_path: Some("/m/b.gguf".into()),
        port: Some(41101),
        launched_by_llamastash: true,
      }],
      gpu: Value::String("CpuOnly".into()),
      host: serde_json::json!({"gpu_backend": "amd", "cpu_pct": 12.5}),
      daemon: None,
      proxy: Value::Null,
    };
    let v = status_json(&snap);
    let model = &v["models"][0];
    assert_eq!(model["launch_id"], serde_json::json!("L1"));
    assert_eq!(model["state"], serde_json::json!("ready"));
    assert_eq!(model["port"], serde_json::json!(41100));
    assert_eq!(
      model["latest_rss_bytes"],
      serde_json::json!(4_500_000_000_u64)
    );
    assert_eq!(model["latest_cpu_pct"], serde_json::json!(312.0));
    let ext = &v["external"][0];
    assert_eq!(ext["pid"], serde_json::json!(999));
  }

  #[test]
  fn status_json_includes_daemon_block_when_present() {
    use crate::cli::resolve::DaemonHealth;
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: Some(DaemonHealth {
        pid: 11,
        uptime_seconds: 7,
        active_connections: 1,
        build: Some("0.1.0".into()),
        server_path: Some("/usr/bin/llama-server".into()),
        ipc_url: Some("http://127.0.0.1:48134".into()),
      }),
      proxy: Value::Null,
    };
    let v = status_json(&snap);
    assert_eq!(v["daemon"]["pid"], serde_json::json!(11));
    assert_eq!(v["daemon"]["uptime_seconds"], serde_json::json!(7));
    assert_eq!(v["daemon"]["active_connections"], serde_json::json!(1));
    assert_eq!(v["daemon"]["build"], serde_json::json!("0.1.0"));
    assert_eq!(
      v["daemon"]["server_path"],
      serde_json::json!("/usr/bin/llama-server")
    );
  }

  #[test]
  fn status_json_preserves_host_block_verbatim() {
    // AGENTS.md guarantees `host` is always an object on the wire.
    // The CLI surface must surface the same shape so agents that
    // parse `status --json` see the same fields as raw IPC clients.
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: serde_json::json!({
        "cpu_pct": 12.5,
        "ram_used_bytes": 1_000_000_u64,
        "ram_total_bytes": 64_000_000_u64,
        "gpu_backend": "amd",
        "gpu_util_pct": 73.0,
        "gpu_temp_c": 62.0,
        "gpu_mem_used_bytes": 3_000_000_000_u64,
        "gpu_mem_total_bytes": 64_000_000_000_u64,
        "gpu_device_count": 1,
      }),
      daemon: None,
      proxy: Value::Null,
    };
    let v = status_json(&snap);
    assert!(v.get("host").is_some(), "host key must appear: {v}");
    assert_eq!(v["host"]["gpu_backend"], serde_json::json!("amd"));
    assert_eq!(v["host"]["cpu_pct"], serde_json::json!(12.5));
    assert_eq!(v["host"]["gpu_device_count"], serde_json::json!(1));
  }

  fn running(launch_id: &str, state: &str, port: u16, path: &str) -> RunningRow {
    RunningRow {
      launch_id: launch_id.into(),
      model_path: path.into(),
      id: None,
      port,
      state: state.into(),
      state_cause: None,
      pid: Some(123),
      mode: "chat".into(),
      ready_at: None,
      params: None,
      latest_rss_bytes: None,
      latest_cpu_pct: None,
    }
  }

  #[test]
  fn status_human_tsv_branch_emits_byte_stable_launches_table() {
    let _g = ColorGuard::set(false);
    let snap = StatusSnapshot {
      models: vec![running("L1", "ready", 41100, "/m/qwen.gguf")],
      external: vec![ExternalRow {
        pid: 9999,
        cmdline: "llama-server".into(),
        model_path: Some("/m/ext.gguf".into()),
        port: None,
        launched_by_llamastash: false,
      }],
      gpu: Value::Null,
      host: Value::Null,
      daemon: None,
      proxy: Value::Null,
    };
    let s = status_human(&snap);
    // Regression guard: managed + external rows are exact tabs, no
    // padding, no color, no truncation.
    assert!(
      s.contains("LAUNCH_ID\tSTATE\tMODE\tPORT\tPID\tNAME\n"),
      "header drifted: {s:?}"
    );
    assert!(s.contains("L1\tready\tchat\t41100\t123\tqwen.gguf\n"));
    assert!(s.contains("external\texternal\t-\t-\t9999\text.gguf\n"));
  }

  #[test]
  fn status_human_tty_branch_pads_launches_table_and_colors_state() {
    let _g = ColorGuard::set(true);
    let snap = StatusSnapshot {
      models: vec![running("L1", "ready", 41100, "/m/qwen.gguf")],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: None,
      proxy: Value::Null,
    };
    let s = status_human(&snap);
    let plain = console::strip_ansi_codes(&s);
    assert!(plain.contains("LAUNCH_ID"), "header missing: {plain:?}");
    assert!(
      !plain.contains("LAUNCH_ID\t"),
      "padded layout must not contain tabs: {plain:?}"
    );
    assert!(plain.contains("L1"));
    assert!(plain.contains("ready"));
    assert!(plain.contains("41100"));
  }

  fn proxy_value(status: &str, listen: Option<&str>, bind_error: Option<&str>) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("enabled".into(), Value::Bool(status != "disabled"));
    obj.insert(
      "listen".into(),
      listen
        .map(|s| Value::String(s.into()))
        .unwrap_or(Value::Null),
    );
    obj.insert("status".into(), Value::String(status.into()));
    obj.insert(
      "bind_error".into(),
      bind_error
        .map(|s| Value::String(s.into()))
        .unwrap_or(Value::Null),
    );
    Value::Object(obj)
  }

  #[test]
  fn status_json_round_trips_proxy_listening_block() {
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: None,
      proxy: proxy_value("listening", Some("127.0.0.1:11434"), None),
    };
    let v = status_json(&snap);
    let proxy = v.get("proxy").expect("proxy block must round-trip");
    assert_eq!(proxy["status"], serde_json::json!("listening"));
    assert_eq!(proxy["listen"], serde_json::json!("127.0.0.1:11434"));
    assert_eq!(proxy["enabled"], serde_json::json!(true));
    assert_eq!(proxy["bind_error"], Value::Null);
  }

  #[test]
  fn status_json_round_trips_proxy_disabled_block() {
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: None,
      proxy: proxy_value("disabled", None, None),
    };
    let v = status_json(&snap);
    let proxy = v.get("proxy").expect("proxy block must round-trip");
    assert_eq!(proxy["status"], serde_json::json!("disabled"));
    assert_eq!(proxy["enabled"], serde_json::json!(false));
    assert_eq!(proxy["listen"], Value::Null);
  }

  #[test]
  fn status_json_round_trips_proxy_port_in_use_block() {
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: None,
      proxy: proxy_value("port_in_use", Some("127.0.0.1:11434"), None),
    };
    let v = status_json(&snap);
    let proxy = v.get("proxy").expect("proxy block must round-trip");
    assert_eq!(proxy["status"], serde_json::json!("port_in_use"));
    assert_eq!(proxy["listen"], serde_json::json!("127.0.0.1:11434"));
    assert_eq!(proxy["bind_error"], Value::Null);
  }

  #[test]
  fn status_json_round_trips_proxy_unbound_block() {
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: None,
      proxy: proxy_value("unbound", Some("127.0.0.1:80"), Some("permission denied")),
    };
    let v = status_json(&snap);
    let proxy = v.get("proxy").expect("proxy block must round-trip");
    assert_eq!(proxy["status"], serde_json::json!("unbound"));
    assert_eq!(proxy["bind_error"], serde_json::json!("permission denied"));
  }

  #[test]
  fn status_json_omits_proxy_block_when_absent() {
    // Pre-Unit-5 daemons emit no `proxy` field. The CLI surface must
    // mirror that by omitting the key entirely, not synthesising a
    // null placeholder.
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: None,
      proxy: Value::Null,
    };
    let v = status_json(&snap);
    assert!(
      v.get("proxy").is_none(),
      "proxy key must be absent when StatusSnapshot.proxy is null: {v}"
    );
  }

  #[test]
  fn status_human_skips_proxy_row_when_disabled() {
    // Plan §Test scenarios edge case 3: disabled config doesn't add a
    // row. The label cycle includes `proxy` as a kv label otherwise;
    // its absence is the signal that the proxy is off.
    use crate::cli::resolve::DaemonHealth;
    let _g = ColorGuard::set(false);
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: Some(DaemonHealth {
        pid: 1,
        uptime_seconds: 0,
        active_connections: 0,
        build: None,
        server_path: None,
        ipc_url: None,
      }),
      proxy: proxy_value("disabled", None, None),
    };
    let s = status_human(&snap);
    assert!(
      !s.contains("proxy"),
      "disabled proxy must not add a `proxy` row: {s:?}"
    );
  }

  #[test]
  fn status_human_renders_proxy_listening_row_under_daemon() {
    use crate::cli::resolve::DaemonHealth;
    let _g = ColorGuard::set(false);
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: Some(DaemonHealth {
        pid: 1,
        uptime_seconds: 0,
        active_connections: 0,
        build: None,
        server_path: None,
        ipc_url: None,
      }),
      proxy: proxy_value("listening", Some("127.0.0.1:11434"), None),
    };
    let s = status_human(&snap);
    assert!(
      s.contains("proxy: listening 127.0.0.1:11434"),
      "expected proxy row, got: {s:?}"
    );
  }

  #[test]
  fn status_human_renders_proxy_port_in_use_row_with_hint() {
    use crate::cli::resolve::DaemonHealth;
    let _g = ColorGuard::set(false);
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: Some(DaemonHealth {
        pid: 1,
        uptime_seconds: 0,
        active_connections: 0,
        build: None,
        server_path: None,
        ipc_url: None,
      }),
      proxy: proxy_value("port_in_use", Some("127.0.0.1:11434"), None),
    };
    let s = status_human(&snap);
    assert!(
      s.contains("port_in_use"),
      "expected port_in_use label: {s:?}"
    );
  }

  #[test]
  fn display_size_sums_every_shard_on_disk_independent_of_cached_weights() {
    // Regression: `list_human` used to read `weights_bytes` straight
    // from the wire, which on a daemon whose catalog predated the
    // split-shard aggregation fix showed only shard 1's bytes.
    // `display_size` must now stat every shard so the SIZE column
    // is self-correcting independent of the daemon's cached value.
    let dir = tempfile::tempdir().unwrap();
    let primary = dir.path().join("m-00001-of-00002.gguf");
    std::fs::write(&primary, vec![0u8; 1024 * 1024]).unwrap(); // 1 MiB
    let sib = dir.path().join("m-00002-of-00002.gguf");
    std::fs::write(&sib, vec![0u8; 2 * 1024 * 1024]).unwrap(); // 2 MiB
    let row = CatalogRow {
      path: primary.display().to_string(),
      // Pretend the daemon cached a way-too-low value (the bug we
      // are working around).
      weights_bytes: Some(1024 * 1024),
      split_siblings: vec![sib.display().to_string()],
      ..row("split", "qwen3", "Q5_K", 32768)
    };
    let rendered = display_size(&row);
    // 1 MiB + 2 MiB = 3 MiB across both shards. format_bytes renders
    // megabytes without a trailing iB suffix, so just check the
    // leading magnitude + M unit.
    assert!(
      rendered.starts_with('3') && rendered.contains('M'),
      "expected ~3 MiB total across shards, got: {rendered}"
    );
  }

  #[test]
  fn display_size_falls_back_to_cached_weights_when_files_missing() {
    let row = CatalogRow {
      path: "/does/not/exist.gguf".into(),
      weights_bytes: Some(42 * 1024 * 1024),
      ..row("ghost", "llama", "Q4_K", 8192)
    };
    let rendered = display_size(&row);
    assert!(
      rendered.starts_with("42") && rendered.contains('M'),
      "expected fallback to cached 42 MiB, got: {rendered}"
    );
  }

  #[test]
  fn status_human_appends_cause_line_for_error_state_launches() {
    let _g = ColorGuard::set(false);
    let mut row = running("L1", "error", 41100, "/m/qwen.gguf");
    row.state_cause = Some("health probe timeout (last status 503); last stderr lines: …".into());
    let snap = StatusSnapshot {
      models: vec![row],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: None,
      proxy: Value::Null,
    };
    let rendered = status_human(&snap);
    assert!(
      rendered.contains("L1 cause:") && rendered.contains("health probe timeout"),
      "expected `L1 cause: …` line beneath the launches table, got:\n{rendered}"
    );
  }

  #[test]
  fn status_json_includes_state_cause_when_set() {
    let mut row = running("L1", "error", 41100, "/m/qwen.gguf");
    row.state_cause = Some("health probe timeout (last status 503)".into());
    let snap = StatusSnapshot {
      models: vec![row],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: None,
      proxy: Value::Null,
    };
    let v = status_json(&snap);
    assert_eq!(
      v["models"][0]["state_cause"],
      serde_json::json!("health probe timeout (last status 503)")
    );
  }

  #[test]
  fn status_json_omits_state_cause_when_none() {
    let snap = StatusSnapshot {
      models: vec![running("L1", "ready", 41100, "/m/qwen.gguf")],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: None,
      proxy: Value::Null,
    };
    let v = status_json(&snap);
    assert!(
      v["models"][0].get("state_cause").is_none(),
      "ready-state rows must not carry a state_cause field"
    );
  }

  #[test]
  fn status_human_renders_proxy_unbound_row_with_cause() {
    use crate::cli::resolve::DaemonHealth;
    let _g = ColorGuard::set(false);
    let snap = StatusSnapshot {
      models: vec![],
      external: vec![],
      gpu: Value::Null,
      host: Value::Null,
      daemon: Some(DaemonHealth {
        pid: 1,
        uptime_seconds: 0,
        active_connections: 0,
        build: None,
        server_path: None,
        ipc_url: None,
      }),
      proxy: proxy_value("unbound", Some("127.0.0.1:80"), Some("permission denied")),
    };
    let s = status_human(&snap);
    assert!(
      s.contains("unbound") && s.contains("permission denied"),
      "expected unbound row with cause: {s:?}"
    );
  }
}
