//! Top-middle info-row pane: daemon socket + pid + uptime,
//! llama-server binary, OpenAI-compat proxy listener, discovery
//! counters, and a one-line running summary.
//!
//! Five label-prefixed rows. Long paths left-truncate with `…/`. The
//! `running` line collapses to `—` when nothing is supervised. Width
//! is flexible — this panel takes whatever's between Host (fixed 32)
//! and Logo (fixed 25 when present). Build version lives on the
//! title row (`render_title_left`) and is not repeated here.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::util::paths::model_display_name;

const LABEL_WIDTH: usize = 8;
const LABEL_SERVER: &str = "server  ";
const LABEL_MODELS: &str = "models  ";
const LABEL_RUNNING: &str = "running ";
const LABEL_PROXY: &str = "proxy   ";
const LABEL_PORT: &str = "port    ";

/// Render the Daemon info panel into `area`. The block title is
/// `Daemon`; inner content is five label-prefixed rows (daemon,
/// server, proxy, counts, running).
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let block = palette.panel_block(" Daemon ", true);
  let inner = block.inner(area);
  frame.render_widget(block, area);

  let row_budget = inner.width.saturating_sub(LABEL_WIDTH as u16) as usize;
  let lines: Vec<Line<'_>> = vec![
    daemon_row(app, row_budget, palette),
    server_row(app, row_budget, palette),
    proxy_row(app, palette),
    counts_row(app, palette),
    running_row(app, row_budget, palette),
  ];
  frame.render_widget(Paragraph::new(lines), inner);
}

/// Render the proxy listener's state. Always on (the third row): when
/// the proxy is up, the row reports `listening 127.0.0.1:<port>` so
/// the live OpenAI-compat endpoint is one glance away; when disabled
/// or absent, the row renders an explicit `disabled` / `—` so the
/// reader can tell the difference between "off by config" and "not
/// reported yet". Labels match the wire `status` values (R161) and
/// the success/error palette signals liveness without forcing the
/// user to read the endpoint.
fn proxy_row<'a>(app: &'a App, palette: &'a Palette) -> Line<'a> {
  let (body, body_style) = match app.daemon_info.proxy.as_ref() {
    None => ("—".to_string(), palette.muted_style()),
    Some(info) => {
      let listen = info.listen.as_deref().unwrap_or("");
      match info.status.as_str() {
        "listening" => (format!("listening {listen}"), palette.success_style()),
        "port_in_use" => (format!("port_in_use {listen}"), palette.error_style()),
        "unbound" => {
          let cause = info.bind_error.as_deref().unwrap_or("bind failed");
          (format!("unbound {listen} ({cause})"), palette.error_style())
        }
        "disabled" => ("disabled".to_string(), palette.muted_style()),
        other => (format!("{other} {listen}"), palette.muted_style()),
      }
    }
  };
  Line::from(vec![
    Span::styled(LABEL_PROXY, palette.label_style()),
    Span::styled(body, body_style),
  ])
}

fn daemon_row<'a>(app: &'a App, _budget: usize, palette: &'a Palette) -> Line<'a> {
  // Layout: `port    48134  pid 1234  up 3h12m`. The panel title is
  // already "Daemon", so a leading `daemon  ` label would just
  // repeat it. The full control-plane URL is loopback-only and the
  // host half is always `127.0.0.1`, so we surface the port (the
  // only operator-relevant chunk) directly. `port` is padded to
  // LABEL_WIDTH so its value column lines up with the other rows;
  // `port`, `pid`, and `up` render in the panel's label colour and
  // numeric values render in text colour.
  let port_val = app
    .daemon_info
    .ipc_url
    .as_deref()
    .and_then(parse_port_from_url)
    .map(|p| p.to_string())
    .unwrap_or_else(|| "—".into());
  let pid_val = app
    .daemon_info
    .pid
    .map(|p| p.to_string())
    .unwrap_or_else(|| "—".into());
  let uptime_val = match (app.daemon_connected, app.daemon_info.uptime_seconds) {
    (true, Some(secs)) => format_uptime(secs),
    _ => "—".into(),
  };

  Line::from(vec![
    Span::styled(LABEL_PORT, palette.label_style()),
    Span::styled(port_val, palette.text_style()),
    Span::styled("  pid ", palette.label_style()),
    Span::styled(pid_val, palette.text_style()),
    Span::styled("  up ", palette.label_style()),
    Span::styled(uptime_val, palette.text_style()),
  ])
}

/// Extract the port from an `http://host:port` URL. Returns `None`
/// when the URL is malformed or no port is present — the caller
/// renders `—` in that case rather than guessing.
fn parse_port_from_url(url: &str) -> Option<u16> {
  let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
  let authority = after_scheme.split('/').next().unwrap_or("");
  authority
    .rsplit_once(':')
    .and_then(|(_, port)| port.parse::<u16>().ok())
}

fn server_row<'a>(app: &'a App, budget: usize, palette: &'a Palette) -> Line<'a> {
  // When the daemon hasn't resolved a `llama-server` binary, point
  // the user at the override knobs instead of a bare `—` — they
  // can install llama.cpp or set LLAMASTASH_LLAMA_SERVER / pass
  // --llama-server to fix it without first guessing where the
  // binary was supposed to live.
  let server_path = app.daemon_info.server_path.as_deref();
  match server_path {
    Some(p) => {
      let flavor_chunk = match flavor_label(app) {
        Some(f) => format!(" ({f})"),
        None => String::new(),
      };
      let path_budget = budget.saturating_sub(flavor_chunk.width());
      let path_truncated = ellipsise(p, path_budget);
      Line::from(vec![
        Span::styled(LABEL_SERVER, palette.label_style()),
        Span::styled(path_truncated, palette.text_style()),
        Span::styled(flavor_chunk, palette.muted_style()),
      ])
    }
    None => {
      let hint = "Not found. Set LLAMASTASH_LLAMA_SERVER or pass --llama-server";
      let trimmed = right_ellipsise(hint, budget);
      Line::from(vec![
        Span::styled(LABEL_SERVER, palette.label_style()),
        Span::styled(trimmed, palette.error_style()),
      ])
    }
  }
}

/// Right-truncate `s` to fit `budget` terminal columns, appending
/// `…` when content was dropped. Use this for free-form text like
/// hints/messages where leading characters carry the most signal;
/// [`ellipsise`] keeps the tail (paths/launch-ids) instead.
fn right_ellipsise(s: &str, budget: usize) -> String {
  if budget == 0 {
    return String::new();
  }
  if s.width() <= budget {
    return s.to_string();
  }
  let keep = budget.saturating_sub(1);
  let mut acc_w = 0usize;
  let mut out = String::with_capacity(keep + 1);
  for ch in s.chars() {
    let w = ch.to_string().width();
    if acc_w + w > keep {
      break;
    }
    out.push(ch);
    acc_w += w;
  }
  out.push('…');
  out
}

fn counts_row<'a>(app: &'a App, palette: &'a Palette) -> Line<'a> {
  let total = app.models.len();
  if total == 0 {
    return Line::from(vec![
      Span::styled(LABEL_MODELS, palette.label_style()),
      Span::styled("no models found", palette.muted_style()),
    ]);
  }
  let ready = app
    .managed
    .iter()
    .filter(|m| matches!(m.state, crate::tui::status_icons::SurfaceState::Ready))
    .count();
  // Count only favorites whose path is in the current catalog so the
  // number matches what the user can actually find in the list. Stale
  // favorites (file deleted / moved out of watched dirs) silently
  // drop off; running favorites still count (their star is visible
  // in the folder group even when the dedicated `★ Favorites`
  // shortcut excludes them).
  let catalog: std::collections::HashSet<&std::path::Path> =
    app.models.iter().map(|m| m.path.as_path()).collect();
  let favorites = app
    .favorites
    .iter()
    .filter(|p| catalog.contains(p.as_path()))
    .count();
  Line::from(vec![
    Span::styled(LABEL_MODELS, palette.label_style()),
    Span::styled(format!("{total} found"), palette.text_style()),
    Span::styled(" · ", palette.muted_style()),
    Span::styled(format!("{ready} ready"), palette.text_style()),
    Span::styled(" · ", palette.muted_style()),
    Span::styled(format!("{favorites} ★"), palette.warning_style()),
  ])
}

fn running_row<'a>(app: &'a App, budget: usize, palette: &'a Palette) -> Line<'a> {
  let n = app.managed.len();
  if n == 0 {
    return Line::from(vec![
      Span::styled(LABEL_RUNNING, palette.label_style()),
      Span::styled("0", palette.muted_style()),
    ]);
  }
  // Layout: `running 3 (name1 :port1 · name2 :port2 · …)`. The
  // count prefix is fixed-width, the paren-list takes the rest and
  // truncates with `…` (not `…/`) since model names read left-to-
  // right; the right edge is the dispensable end.
  let prefix = format!("{n} (");
  let suffix = ")";
  let prefix_w = prefix.width();
  let suffix_w = suffix.width();
  let list_budget = budget.saturating_sub(prefix_w + suffix_w);
  let parts: Vec<String> = app
    .managed
    .iter()
    .map(|m| {
      let label = app
        .display_label_for(&m.path)
        .unwrap_or_else(|| model_display_name(&m.path));
      format!("{label} :{}", m.port)
    })
    .collect();
  let joined = parts.join(" · ");
  let trimmed = right_ellipsise(&joined, list_budget);
  Line::from(vec![
    Span::styled(LABEL_RUNNING, palette.label_style()),
    Span::styled(prefix, palette.text_style()),
    Span::styled(trimmed, palette.text_style()),
    Span::styled(suffix, palette.text_style()),
  ])
}

/// Backend → human-readable flavor tag used in the `server` row.
///
/// The tag reflects the llama.cpp backend the auto-installer picks for
/// this host (see `init::install::gh_releases::pick_asset_suffix`). AMD
/// is OS-dependent: Linux installs the ROCm build, but Windows installs
/// the **Vulkan** build (ROCm's Windows support is too narrow to pick
/// blindly), so labeling a Windows AMD server `rocm` is wrong.
fn flavor_label(app: &App) -> Option<&'static str> {
  use crate::daemon::host_metrics::GpuFlavor;
  match app.host_metrics.flavor() {
    GpuFlavor::Nvidia => Some("cuda"),
    GpuFlavor::Amd => Some(if cfg!(target_os = "windows") {
      "vulkan"
    } else {
      "rocm"
    }),
    GpuFlavor::AppleMetal => Some("metal"),
    GpuFlavor::CpuOnly => Some("cpu"),
    GpuFlavor::Unknown | GpuFlavor::Unsampled => None,
  }
}

/// Truncate `s` to fit `budget` terminal columns from the **left**,
/// prepending `…/` so the trailing component (the binary name, the
/// launch id) stays visible. Returns the original string unmodified
/// when it already fits.
///
/// Measures in unicode display width (`UnicodeWidthStr`), not char
/// count: a CJK character occupies two cells, so on a CJK install
/// prefix like `/Users/张伟/.../llama-server` char-count truncation
/// would overflow the reserved budget and push the trailing flavor
/// chunk off-screen.
fn ellipsise(s: &str, budget: usize) -> String {
  if budget == 0 {
    return String::new();
  }
  if s.width() <= budget {
    return s.to_string();
  }
  let prefix = "…/";
  let prefix_w = prefix.width();
  if budget <= prefix_w {
    return take_tail_by_width(s, budget);
  }
  let keep_budget = budget - prefix_w;
  let tail = take_tail_by_width(s, keep_budget);
  format!("{prefix}{tail}")
}

/// Take the longest suffix of `s` whose display width is `<= budget`.
/// Iterates chars in reverse so a wide character that wouldn't fit is
/// dropped rather than splitting it.
fn take_tail_by_width(s: &str, budget: usize) -> String {
  let mut acc_w = 0usize;
  let mut start = s.len();
  for (idx, ch) in s.char_indices().rev() {
    let w = ch.to_string().width();
    if acc_w + w > budget {
      break;
    }
    acc_w += w;
    start = idx;
  }
  s[start..].to_string()
}

fn format_uptime(secs: u64) -> String {
  let hours = secs / 3600;
  let minutes = (secs % 3600) / 60;
  if hours > 0 {
    format!("{hours}h{minutes:02}m")
  } else {
    format!("{minutes}m")
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::daemon::host_metrics::HostMetricsSnapshot;
  use crate::discovery::{DiscoveredModel, ModelSource};
  use crate::gguf::metadata::{ModeHint, ModelMetadata, Quant};
  use crate::tui::app::{App, AppOptions, DaemonInfo, ManagedRow};
  use crate::tui::status_icons::SurfaceState;
  use ratatui::backend::TestBackend;
  use ratatui::Terminal;
  use std::path::PathBuf;

  fn fake_model(path: &str, parent: &str) -> DiscoveredModel {
    DiscoveredModel {
      path: PathBuf::from(path),
      parent: PathBuf::from(parent),
      source: ModelSource::UserPath,
      metadata: Some(ModelMetadata {
        arch: Some("llama".into()),
        total_parameters: Some(7_000_000_000),
        parameter_label: Some("7B".into()),
        quant: Quant::Q4_K,
        native_ctx: Some(8192),
        chat_template: None,
        tokenizer_kind: None,
        reasoning_hint: false,
        mode_hint: ModeHint::Chat,
        weights_bytes: Some(4_200_000_000),
      }),
      parse_error: None,
      split_siblings: Vec::new(),
      display_label: None,
    }
  }

  fn fake_managed(path: &str, port: u16, state: SurfaceState) -> ManagedRow {
    ManagedRow {
      launch_id: "L".into(),
      path: PathBuf::from(path),
      port,
      state,
      rss_bytes: Some(4_200_000_000),
      cpu_pct: Some(312.0),
    }
  }

  fn render_lines(app: &App) -> Vec<String> {
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(50, 7)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 50, 7), app, palette))
      .unwrap();
    let buf = term.backend().buffer().clone();
    let mut rows: Vec<String> = Vec::new();
    for y in 0..buf.area.height {
      let mut row = String::new();
      for x in 0..buf.area.width {
        row.push_str(buf.cell((x, y)).unwrap().symbol());
      }
      rows.push(row.trim_end().to_string());
    }
    rows
  }

  #[test]
  fn ellipsise_pads_short_strings_unchanged() {
    assert_eq!(ellipsise("ok", 10), "ok");
    assert_eq!(ellipsise("", 10), "");
  }

  #[test]
  fn ellipsise_left_truncates_long_paths() {
    let truncated = ellipsise("/usr/local/lib/llama-cpp-cuda/bin/llama-server", 20);
    assert!(truncated.starts_with("…/"));
    assert!(truncated.ends_with("llama-server"));
    assert!(truncated.width() <= 20);
  }

  #[test]
  fn ellipsise_measures_in_display_columns_not_char_count() {
    // CJK characters occupy two terminal cells each. A char-count
    // implementation would let `/张伟/llama-server` (15 chars, 17
    // cells) "fit" inside a 16-column budget; ratatui would then
    // clip the flavor chunk that the caller appends. Width-based
    // measurement keeps the truncated string inside the requested
    // budget so the trailing chunk renders intact.
    let s = "/usr/local/张伟/bin/llama-server";
    let out = ellipsise(s, 20);
    assert!(
      out.width() <= 20,
      "expected width <= 20, got {} for {out:?}",
      out.width()
    );
    assert!(out.ends_with("llama-server"));
  }

  #[test]
  fn format_uptime_omits_hours_when_zero() {
    assert_eq!(format_uptime(0), "0m");
    assert_eq!(format_uptime(45), "0m");
    assert_eq!(format_uptime(60), "1m");
    assert_eq!(format_uptime(3600), "1h00m");
    assert_eq!(format_uptime(3 * 3600 + 12 * 60), "3h12m");
  }

  #[test]
  fn counts_row_renders_no_models_message_when_empty() {
    let app = App::new(AppOptions::default());
    let rows = render_lines(&app);
    assert!(rows.iter().any(|r| r.contains("no models found")));
  }

  #[test]
  fn counts_row_renders_total_ready_favorites() {
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model("/m/a.gguf", "/m")];
    app.managed = vec![fake_managed("/m/a.gguf", 41100, SurfaceState::Ready)];
    app.favorites = vec![PathBuf::from("/m/a.gguf")];
    let rows = render_lines(&app);
    assert!(
      rows.iter().any(|r| r.contains("1 found")),
      "counts row missing `1 found`: {rows:#?}"
    );
    assert!(rows.iter().any(|r| r.contains("1 ready")));
    assert!(rows.iter().any(|r| r.contains("1 ★")));
  }

  #[test]
  fn counts_row_drops_stale_favorites_not_in_catalog() {
    // A favorite whose path is no longer in `app.models` (file deleted
    // or moved out of watched dirs) must NOT inflate the `N ★` count —
    // the list pane would have no row to render it on, so reporting it
    // here desyncs the count from what's actually visible.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model("/m/a.gguf", "/m")];
    app.favorites = vec![
      PathBuf::from("/m/a.gguf"),     // in catalog
      PathBuf::from("/m/ghost.gguf"), // stale — not in catalog
    ];
    let rows = render_lines(&app);
    assert!(
      rows.iter().any(|r| r.contains("1 ★")),
      "stale favorite must drop from the count: {rows:#?}"
    );
    assert!(
      !rows.iter().any(|r| r.contains("2 ★")),
      "count must not include stale favorites: {rows:#?}"
    );
  }

  #[test]
  fn counts_row_includes_running_favorites() {
    // A favorited model that's currently running still counts — the
    // user has it favorited, the catalog still surfaces it, and the
    // star is visible on the row in its folder group. Each model
    // counts exactly once.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model("/m/a.gguf", "/m"), fake_model("/m/b.gguf", "/m")];
    app.managed = vec![fake_managed("/m/a.gguf", 41100, SurfaceState::Ready)];
    app.favorites = vec![PathBuf::from("/m/a.gguf"), PathBuf::from("/m/b.gguf")];
    let rows = render_lines(&app);
    assert!(
      rows.iter().any(|r| r.contains("2 ★")),
      "running favorite must still count: {rows:#?}"
    );
  }

  #[test]
  fn daemon_row_shows_em_dash_uptime_when_daemon_disconnected() {
    let mut app = App::new(AppOptions::default());
    app.daemon_connected = false;
    app.daemon_info = DaemonInfo {
      uptime_seconds: Some(120),
      pid: Some(1234),
      ..Default::default()
    };
    let rows = render_lines(&app);
    // The uptime chunk should read `up —`, not `up 2m`, because the
    // connection went down — the cached value is stale.
    let daemon_row = rows.iter().find(|r| r.contains("port ")).unwrap();
    assert!(
      daemon_row.contains("up —"),
      "expected `up —` chunk when daemon disconnected, got: {daemon_row:?}"
    );
  }

  #[test]
  fn daemon_row_shows_uptime_chunk_when_daemon_connected() {
    let mut app = App::new(AppOptions::default());
    app.daemon_connected = true;
    app.daemon_info = DaemonInfo {
      uptime_seconds: Some(3 * 3600 + 12 * 60),
      pid: Some(1234),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let daemon_row = rows.iter().find(|r| r.contains("port ")).unwrap();
    assert!(
      daemon_row.contains("up 3h12m"),
      "expected `up 3h12m` chunk, got: {daemon_row:?}"
    );
  }

  #[test]
  fn running_row_shows_zero_when_no_managed_rows() {
    let app = App::new(AppOptions::default());
    let rows = render_lines(&app);
    let running_row = rows.iter().find(|r| r.contains("running")).unwrap();
    assert!(
      running_row.contains("0") && !running_row.contains("("),
      "expected `running 0` (no parens) when none managed, got: {running_row:?}"
    );
  }

  #[test]
  fn running_row_lists_each_managed_with_count_prefix() {
    let mut app = App::new(AppOptions::default());
    app.managed = vec![
      fake_managed("/m/qwen.gguf", 41100, SurfaceState::Ready),
      fake_managed("/m/gemma.gguf", 41101, SurfaceState::Loading),
    ];
    let rows = render_lines(&app);
    let running_row = rows.iter().find(|r| r.contains("running")).unwrap();
    assert!(
      running_row.contains("2 ("),
      "expected count prefix `2 (`, got: {running_row:?}"
    );
    assert!(
      running_row.contains("qwen :41100"),
      "expected `qwen :41100` entry, got: {running_row:?}"
    );
  }

  #[test]
  fn running_row_truncates_with_ellipsis_when_list_overflows() {
    let mut app = App::new(AppOptions::default());
    app.managed = (0..5)
      .map(|i| {
        fake_managed(
          &format!("/m/model-with-very-long-name-{i}"),
          41100 + i,
          SurfaceState::Ready,
        )
      })
      .collect();
    let rows = render_lines(&app);
    let running_row = rows.iter().find(|r| r.contains("running")).unwrap();
    assert!(
      running_row.contains("…"),
      "expected trailing `…` on overflow, got: {running_row:?}"
    );
    // The count prefix must always survive truncation.
    assert!(
      running_row.contains("5 ("),
      "expected `5 (` count prefix even on overflow, got: {running_row:?}"
    );
  }

  #[test]
  fn server_row_emits_flavor_from_host_metrics_backend() {
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      server_path: Some("/usr/bin/llama-server".into()),
      ..Default::default()
    };
    app.host_metrics = HostMetricsSnapshot {
      gpu_backend: "nvidia".into(),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let server_row = rows.iter().find(|r| r.contains("server")).unwrap();
    assert!(
      server_row.contains("(cuda)"),
      "expected `(cuda)` flavor on nvidia backend, got: {server_row:?}"
    );
  }

  #[test]
  fn server_row_picks_install_flavor_for_amd_backend() {
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      server_path: Some("/usr/bin/llama-server".into()),
      ..Default::default()
    };
    app.host_metrics = HostMetricsSnapshot {
      gpu_backend: "amd".into(),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let server_row = rows.iter().find(|r| r.contains("server")).unwrap();
    // Windows AMD installs the Vulkan build, not ROCm (see flavor_label).
    let expected = if cfg!(target_os = "windows") {
      "(vulkan)"
    } else {
      "(rocm)"
    };
    assert!(server_row.contains(expected), "{server_row:?}");
  }

  #[test]
  fn server_row_picks_metal_flavor_for_apple_metal_backend() {
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      server_path: Some("/usr/bin/llama-server".into()),
      ..Default::default()
    };
    app.host_metrics = HostMetricsSnapshot {
      gpu_backend: "apple_metal".into(),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let server_row = rows.iter().find(|r| r.contains("server")).unwrap();
    assert!(server_row.contains("(metal)"), "{server_row:?}");
  }

  #[test]
  fn server_row_omits_flavor_chunk_for_unsampled_backend() {
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      server_path: Some("/usr/bin/llama-server".into()),
      ..Default::default()
    };
    app.host_metrics = HostMetricsSnapshot {
      gpu_backend: "unsampled".into(),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let server_row = rows.iter().find(|r| r.contains("server")).unwrap();
    // No parenthesised flavor chunk should appear yet.
    assert!(
      !server_row.contains('('),
      "unsampled backend should suppress the flavor chunk: {server_row:?}"
    );
  }

  #[test]
  fn daemon_row_renders_pid_when_daemon_info_carries_one() {
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      pid: Some(1234),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let daemon_row = rows.iter().find(|r| r.contains("port ")).unwrap();
    assert!(
      daemon_row.contains("pid 1234"),
      "expected `pid 1234`, got: {daemon_row:?}"
    );
  }

  #[test]
  fn daemon_row_renders_port_chunk_when_ipc_url_present() {
    // The row distils `ipc_url` down to its port — the loopback host
    // is constant (`127.0.0.1`) so showing it on every render wastes
    // panel real estate that narrow terminals need for the pid/up
    // chunks.
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      pid: Some(4242),
      ipc_url: Some("http://127.0.0.1:48134".into()),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let daemon_row = rows.iter().find(|r| r.contains("port ")).unwrap();
    assert!(
      daemon_row.contains("port    48134"),
      "expected `port    48134` chunk (label padded to LABEL_WIDTH), got: {daemon_row:?}"
    );
    assert!(
      !daemon_row.contains("127.0.0.1"),
      "host half should not bleed into the row: {daemon_row:?}"
    );
    assert!(
      daemon_row.contains("pid 4242"),
      "pid must remain on the row alongside the port: {daemon_row:?}"
    );
  }

  #[test]
  fn daemon_row_renders_em_dash_port_when_ipc_url_missing() {
    // A pre-Phase-A daemon (or a status response that hasn't landed
    // yet) leaves `ipc_url = None`. The row must still render a
    // placeholder rather than collapsing the `port` chunk away — the
    // reader needs the same five fixed rhythms to keep their eye on
    // pid + uptime.
    let mut app = App::new(AppOptions::default());
    app.daemon_connected = true;
    app.daemon_info = DaemonInfo {
      pid: Some(4242),
      uptime_seconds: Some(60),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let daemon_row = rows.iter().find(|r| r.contains("port ")).unwrap();
    assert!(
      daemon_row.contains("port    —"),
      "expected `port    —` placeholder (label padded to LABEL_WIDTH), got: {daemon_row:?}"
    );
  }

  #[test]
  fn parse_port_from_url_handles_well_formed_urls() {
    assert_eq!(parse_port_from_url("http://127.0.0.1:48134"), Some(48134));
    assert_eq!(parse_port_from_url("http://127.0.0.1:48134/"), Some(48134));
    assert_eq!(parse_port_from_url("http://localhost:8080/rpc"), Some(8080));
  }

  #[test]
  fn parse_port_from_url_returns_none_when_port_absent_or_bad() {
    // No `:port` segment — caller renders `—`.
    assert_eq!(parse_port_from_url("http://127.0.0.1"), None);
    // Non-numeric port.
    assert_eq!(parse_port_from_url("http://127.0.0.1:abc"), None);
    // Out-of-range (u16 overflow).
    assert_eq!(parse_port_from_url("http://127.0.0.1:99999"), None);
  }

  #[test]
  fn server_row_shows_actionable_hint_when_binary_unresolved() {
    // When the daemon hasn't located a `llama-server` binary, the
    // row tells the user what knobs to turn — rather than a bare
    // `—` that reads as a bug. The hint mentions both the env var
    // and the CLI flag so either fix is discoverable.
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      server_path: None,
      ..Default::default()
    };
    app.host_metrics = HostMetricsSnapshot {
      gpu_backend: "amd".into(),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let server_row = rows.iter().find(|r| r.contains("server")).unwrap();
    // The narrow 50-cell test backend truncates the trailing
    // `--llama-server` suggestion, but the leading "Not found"
    // tag and the full `LLAMASTASH_LLAMA_SERVER` env-var name
    // survive — both are actionable on their own.
    assert!(
      server_row.contains("Not found"),
      "expected `Not found` hint, got: {server_row:?}"
    );
    assert!(
      server_row.contains("LLAMASTASH"),
      "expected LLAMASTASH env-var hint, got: {server_row:?}"
    );
    assert!(
      !server_row.contains('('),
      "flavor must be suppressed when binary is unresolved: {server_row:?}"
    );
  }

  #[test]
  fn proxy_row_renders_listening_endpoint_when_set() {
    use crate::tui::app::ProxyInfo;
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      proxy: Some(ProxyInfo {
        enabled: true,
        listen: Some("127.0.0.1:11434".into()),
        status: "listening".into(),
        bind_error: None,
      }),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let proxy_row = rows
      .iter()
      .find(|r| r.contains("proxy"))
      .expect("proxy row must render when daemon_info.proxy is set");
    assert!(
      proxy_row.contains("listening 127.0.0.1:11434"),
      "expected `listening 127.0.0.1:11434`: {proxy_row:?}"
    );
  }

  #[test]
  fn proxy_row_renders_port_in_use_with_endpoint() {
    use crate::tui::app::ProxyInfo;
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      proxy: Some(ProxyInfo {
        enabled: true,
        listen: Some("127.0.0.1:11434".into()),
        status: "port_in_use".into(),
        bind_error: None,
      }),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let proxy_row = rows.iter().find(|r| r.contains("proxy")).unwrap();
    assert!(
      proxy_row.contains("port_in_use"),
      "expected `port_in_use` label: {proxy_row:?}"
    );
  }

  #[test]
  fn proxy_row_renders_em_dash_when_no_proxy_info() {
    // The proxy row is always present in the 5-row layout; when the
    // daemon hasn't reported a proxy block yet (or a pre-Unit-5
    // daemon omits it entirely), the row reads `proxy   —` instead
    // of being hidden — so the reader can tell "not reported" apart
    // from `disabled` (config off) or `listening` (live).
    let app = App::new(AppOptions::default());
    let rows = render_lines(&app);
    let proxy_row = rows
      .iter()
      .find(|r| r.contains("proxy"))
      .expect("proxy row must always render");
    assert!(
      proxy_row.contains("—"),
      "expected em-dash placeholder when daemon_info.proxy is None: {proxy_row:?}"
    );
  }

  #[test]
  fn proxy_row_renders_disabled_when_config_disabled() {
    use crate::tui::app::ProxyInfo;
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      proxy: Some(ProxyInfo {
        enabled: false,
        listen: None,
        status: "disabled".into(),
        bind_error: None,
      }),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let proxy_row = rows.iter().find(|r| r.contains("proxy")).unwrap();
    assert!(
      proxy_row.contains("disabled"),
      "expected `disabled` body for disabled status, got: {proxy_row:?}"
    );
  }

  #[test]
  fn server_and_proxy_rows_render_simultaneously() {
    use crate::tui::app::ProxyInfo;
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      server_path: Some("/usr/bin/llama-server".into()),
      proxy: Some(ProxyInfo {
        enabled: true,
        listen: Some("127.0.0.1:11434".into()),
        status: "listening".into(),
        bind_error: None,
      }),
      ..Default::default()
    };
    let rows = render_lines(&app);
    assert!(
      rows
        .iter()
        .any(|r| r.contains("server") && r.contains("llama-server")),
      "expected server row alongside the proxy row: {rows:#?}"
    );
    assert!(
      rows
        .iter()
        .any(|r| r.contains("proxy") && r.contains("listening 127.0.0.1:11434")),
      "expected proxy row alongside the server row: {rows:#?}"
    );
  }
}
