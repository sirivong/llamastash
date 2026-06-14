//! Top-left info-row pane: host CPU / MEM / GPU / VRAM bar gauges
//! plus a backend tag line.
//!
//! Layout (32 cols × 5 inner rows by default):
//!
//! ```text
//! CPU  ███████░░░ 58% 71°C
//! MEM  █████░░░░░ 11.4/32 G
//! GPU  ██████████ 84% 68°C
//! VRAM ███████░░░ 14.2/24 G
//! backend  NVIDIA · 1 GPU
//! ```
//!
//! `MEM` is system RAM; `MEM*` marks unified memory (Apple Silicon,
//! AMD/Intel UMA APUs) where the GPU draws from that same pool. On a
//! discrete card the `VRAM` row reads `used / total`; on unified memory
//! the denominator is the GPU's *reachable* ceiling given current RAM
//! pressure (`pool − non-GPU RAM use`), not the static GTT cap — see
//! `vram_denominator`.
//!
//! Backend-specific variants:
//! * Apple Silicon (unified memory): CPU + `MEM*` + a
//!   `GPU  unified` text row.
//! * `CpuOnly`: CPU + MEM only, GPU + VRAM rows omitted.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::daemon::host_metrics::HostMetricsSnapshot;
use crate::theme::Palette;
use crate::tui::fmt::format_bytes_pair;

const LABEL_WIDTH: usize = 5;

/// Render the Host stats pane into `area`.
pub fn render(frame: &mut Frame<'_>, area: Rect, host: &HostMetricsSnapshot, palette: &Palette) {
  let block = palette.panel_block(" Host ", true);
  let inner = block.inner(area);
  frame.render_widget(block, area);

  let bar_width = bar_width_for(inner.width);
  let mut lines: Vec<Line<'_>> = Vec::with_capacity(5);

  // CPU row is always present.
  lines.push(cpu_row(host, bar_width, palette));
  // RAM row is always present (Apple Silicon labels it "(unified)").
  lines.push(ram_row(host, bar_width, palette));
  // GPU rows depend on backend. The empty-string and "unsampled"
  // values both mean "no readings yet" — render the same collapsed
  // layout as cpu_only so the pre-first-tick window doesn't show
  // four bars filled to 0%.
  match host.gpu_backend.as_str() {
    s if s == HostMetricsSnapshot::BACKEND_CPU_ONLY
      || s == HostMetricsSnapshot::UNINITIALIZED_BACKEND
      || s.is_empty() =>
    {
      // No GPU rows; one blank line preserves vertical rhythm so
      // the backend label lands on the same row across variants.
      lines.push(Line::from(""));
    }
    s if s == HostMetricsSnapshot::BACKEND_APPLE_METAL => {
      lines.push(Line::from(vec![
        Span::styled("GPU  ", palette.label_style()),
        Span::styled("unified", palette.text_style()),
      ]));
    }
    _ => {
      // Two or more GPUs: render one row per device so the user
      // sees each card's utilization and temperature.
      // One GPU: keep the current single-row layout.
      let n_devices = host.gpu_devices.as_ref().map_or(1, |ds| ds.len());
      if n_devices > 1 {
        for gpu_line in gpu_device_rows(host, bar_width, palette) {
          lines.push(gpu_line);
        }
      } else {
        lines.push(gpu_util_row(host, bar_width, palette));
      }
      // The `MEM*` marker on the memory row already flags that the GPU
      // shares that pool, so the GPU memory keeps its own `VRAM` gauge.
      lines.push(vram_row(host, bar_width, palette));
    }
  }

  // Pad to four content rows so the backend label always sits on row 5.
  while lines.len() < 4 {
    lines.push(Line::from(""));
  }
  lines.push(backend_row(host, palette));
  frame.render_widget(Paragraph::new(lines), inner);
}

fn cpu_row<'a>(host: &HostMetricsSnapshot, bar_width: usize, palette: &'a Palette) -> Line<'a> {
  let pct = host.cpu_pct.clamp(0.0, 100.0);
  let bar = bar(pct, bar_width, gauge_color(pct, palette));
  // Values land in a left-aligned column after the bar so CPU /
  // RAM / GPU / VRAM all start at the same screen offset.
  let value = format!(" {:.0}%", host.cpu_pct);
  let mut spans = vec![
    Span::styled("CPU  ", palette.label_style()),
    bar,
    Span::styled(value, palette.text_style()),
  ];
  // CPU temperature renders next to the percent so the row reads
  // symmetrically with the GPU row, when sysinfo's component
  // sensor surfaced a reading. Same colour tiers as GPU temp.
  if let Some(temp) = host.cpu_temp_c {
    spans.push(Span::raw(" "));
    spans.extend(temp_spans(temp, palette));
  }
  Line::from(spans)
}

fn ram_row<'a>(host: &HostMetricsSnapshot, bar_width: usize, palette: &'a Palette) -> Line<'a> {
  // Always show the true system RAM total (the same number `init`
  // reports). On unified-memory machines the GPU's allocation lives in
  // this pool, but `sysinfo`'s used/total already account for it — the
  // earlier subtraction of `uma_shared_*` produced a wrong RAM value
  // (and, when DXGI mis-flagged discrete cards, a spurious `*`). The
  // `unified` flag below is what tells the user the pool is shared.
  let (pct, value) = if host.ram_total_bytes == 0 {
    (0.0_f32, "—/—".to_string())
  } else {
    let pct = (host.ram_used_bytes as f64 / host.ram_total_bytes as f64) as f32 * 100.0;
    (
      pct.clamp(0.0, 100.0),
      format_bytes_pair(host.ram_used_bytes, host.ram_total_bytes),
    )
  };
  // `MEM*` flags unified memory (Apple Silicon + AMD/Intel UMA APUs)
  // where the GPU draws from this same pool; `MEM` is plain system RAM.
  // Sourced from the one `GpuInfo::is_unified` helper init shares, so
  // the marker can't drift between the two render paths.
  let label = if host.unified { "MEM* " } else { "MEM  " };
  let bar = bar(pct, bar_width, gauge_color(pct, palette));
  Line::from(vec![
    Span::styled(label, palette.label_style()),
    bar,
    Span::styled(format!(" {value}"), palette.text_style()),
  ])
}

fn gpu_util_row<'a>(
  host: &HostMetricsSnapshot,
  bar_width: usize,
  palette: &'a Palette,
) -> Line<'a> {
  let pct = host.gpu_util_pct.unwrap_or(0.0).clamp(0.0, 100.0);
  let bar = bar(pct, bar_width, gauge_color(pct, palette));
  // Same left-aligned-column treatment as the CPU row: no leading
  // pad so values line up with `CPU 3%`, `RAM 31G/62G`, etc.
  let value = host
    .gpu_util_pct
    .map(|p| format!(" {:.0}%", p))
    .unwrap_or_else(|| " —".into());
  let mut spans = vec![
    Span::styled("GPU  ", palette.label_style()),
    bar,
    Span::styled(value, palette.text_style()),
  ];
  if let Some(temp) = host.gpu_temp_c {
    spans.push(Span::raw(" "));
    spans.extend(temp_spans(temp, palette));
  }
  Line::from(spans)
}

/// Render one GPU row per device (for multi-GPU machines).
fn gpu_device_rows<'a>(
  host: &HostMetricsSnapshot,
  bar_width: usize,
  palette: &'a Palette,
) -> Vec<Line<'a>> {
  let mut lines: Vec<Line<'a>> = Vec::new();
  if let Some(devices) = &host.gpu_devices {
    for (i, dev) in devices.iter().enumerate() {
      let pct = dev.utilization_pct.unwrap_or(0.0).clamp(0.0, 100.0);
      let bar = bar(pct, bar_width, gauge_color(pct, palette));
      let label = if devices.len() == 1 {
        "GPU  ".into()
      } else {
        format!("GPU{} ", i)
      };
      let value = dev
        .utilization_pct
        .map(|p| format!(" {:.0}%", p))
        .unwrap_or_else(|| " —".into());
      let mut spans = vec![
        Span::styled(label, palette.label_style()),
        bar,
        Span::styled(value, palette.text_style()),
      ];
      if let Some(temp) = dev.temperature_c {
        spans.push(Span::raw(" "));
        spans.extend(temp_spans(temp, palette));
      }
      lines.push(Line::from(spans));
    }
  }
  lines
}

/// Effective denominator for the VRAM gauge.
///
/// Discrete cards own a dedicated pool, so the gauge is `used / total`.
/// On unified memory the GPU and CPU share one physical pool: the GPU
/// can only reach what the rest of the system isn't already holding, so
/// the honest ceiling is `pool_total − (ram_used − gpu_in_shared)` — the
/// pool minus *non-GPU* RAM use. Subtracting the bare `ram_used` would
/// double-count the GPU's own bytes (sysinfo already folds the GTT
/// allocation into `ram_used`), so we add `gpu_in_shared` back first.
/// Clamped to `>= used` so the gauge never reads over 100%.
fn vram_denominator(host: &HostMetricsSnapshot, used: u64, total: u64) -> u64 {
  if !host.unified {
    return total;
  }
  let gpu_in_shared = host.uma_shared_used_bytes.unwrap_or(used);
  let other_ram = host.ram_used_bytes.saturating_sub(gpu_in_shared);
  total.saturating_sub(other_ram).max(used)
}

fn vram_row<'a>(host: &HostMetricsSnapshot, bar_width: usize, palette: &'a Palette) -> Line<'a> {
  let (pct, value) = match (host.gpu_mem_used_bytes, host.gpu_mem_total_bytes) {
    (Some(used), Some(total)) if total > 0 => {
      // On UMA the denominator is the GPU's reachable ceiling given
      // current RAM pressure, not the static GTT cap.
      let avail = vram_denominator(host, used, total);
      let pct = (used as f64 / avail as f64) as f32 * 100.0;
      (pct.clamp(0.0, 100.0), format_bytes_pair(used, avail))
    }
    _ => (0.0_f32, "—/—".into()),
  };
  let bar = bar(pct, bar_width, gauge_color(pct, palette));
  Line::from(vec![
    Span::styled("VRAM ", palette.label_style()),
    bar,
    Span::styled(format!(" {value}"), palette.text_style()),
  ])
}

fn backend_row<'a>(host: &HostMetricsSnapshot, palette: &'a Palette) -> Line<'a> {
  use crate::daemon::host_metrics::GpuFlavor;
  use crate::init::detection::gpu_vendor_display;
  let label = match host.flavor() {
    // Name the vendor (AMD / NVIDIA / Apple) consistently with
    // `status`, `doctor`, and `init` — not the metrics tool (NVML /
    // ROCm), which conflated the vendor with the llama.cpp runtime.
    GpuFlavor::Nvidia | GpuFlavor::Amd => format!(
      "{} · {}",
      gpu_vendor_display(&host.gpu_backend),
      pluralize_gpu(host.gpu_device_count)
    ),
    GpuFlavor::AppleMetal => "Apple · 1 GPU".into(),
    GpuFlavor::CpuOnly => "CPU only".into(),
    GpuFlavor::Unsampled => "detecting".into(),
    GpuFlavor::Multi => {
      // Two+ backends found GPUs — show combined count.
      let mut nvidia_count = 0u32;
      let mut amd_count = 0u32;
      let mut unknown_count = 0u32;
      let mut metal_count = 0u32;
      if let Some(devices) = &host.gpu_devices {
        for dev in devices {
          match dev.selector.chars().next() {
            Some('N') => nvidia_count += 1,
            Some('A') => amd_count += 1,
            Some('V') => unknown_count += 1,
            Some('M') => metal_count += 1,
            _ => unknown_count += 1,
          }
        }
      }
      let parts: Vec<String> = vec![
        if nvidia_count > 0 {
          Some(format!("NVIDIA · {}", pluralize_gpu(nvidia_count)))
        } else {
          None
        },
        if amd_count > 0 {
          Some(format!("AMD · {}", pluralize_gpu(amd_count)))
        } else {
          None
        },
        if metal_count > 0 {
          Some("Apple · 1 GPU".into())
        } else {
          None
        },
        if unknown_count > 0 {
          Some(format!("GPU (Vulkan) · {}", pluralize_gpu(unknown_count)))
        } else {
          None
        },
      ]
      .into_iter()
      .flatten()
      .collect();
      parts.join(" + ")
    }
    // Pass the raw label through so a future backend label not yet
    // classified by `GpuFlavor` still surfaces a debuggable string
    // (rather than a generic "unknown") in the Host pane.
    GpuFlavor::Unknown => host.gpu_backend.clone(),
  };
  Line::from(vec![
    Span::styled("backend  ", palette.label_style()),
    Span::styled(label, palette.text_style()),
  ])
}

fn pluralize_gpu(n: u32) -> String {
  if n == 1 {
    "1 GPU".into()
  } else {
    format!("{n} GPUs")
  }
}

/// Compute the bar width — 60% of the inner area, clamped to a usable
/// range so the trailing percent / units column always has room.
fn bar_width_for(inner_width: u16) -> usize {
  let budget = inner_width as usize;
  // Leave space for label + percent/value/temp. The 11-cell reserve
  // covers the widest right-of-bar payload (" 100% ▲82°C" — temp rows
  // carry a 1-cell severity glyph in the warning/critical tiers so the
  // bar doesn't shift width as temps cross thresholds).
  let usable = budget.saturating_sub(LABEL_WIDTH + 11);
  usable.clamp(4, 14)
}

/// Render a single bar `[████░░░░]` of `width` cells. Fill chars are
/// `█`, trough chars `░`. We can't color two halves of one span
/// without splitting, so the fill color owns the whole string — the
/// trough chars naturally read as a dimmer shade of the fill because
/// `░` is a 25%-density glyph. (Matches kdash's visual.)
fn bar(pct: f32, width: usize, fill: Color) -> Span<'static> {
  if width == 0 {
    return Span::raw("");
  }
  let filled = ((pct / 100.0) * width as f32).round() as usize;
  let filled = filled.min(width);
  let trough = width - filled;
  let mut s = String::with_capacity(width * 3);
  for _ in 0..filled {
    s.push('█');
  }
  for _ in 0..trough {
    s.push('░');
  }
  Span::styled(s, Style::default().fg(fill))
}

/// Gauge color tier: green ≤60%, yellow 60–85%, red ≥85%.
fn gauge_color(pct: f32, palette: &Palette) -> Color {
  if pct >= 85.0 {
    palette.error
  } else if pct >= 60.0 {
    palette.warning
  } else {
    palette.success
  }
}

/// GPU temperature tier: green ≤70°C, yellow 70–82°C, red ≥82°C.
fn gpu_temp_color(temp: f32, palette: &Palette) -> Color {
  if temp >= 82.0 {
    palette.error
  } else if temp >= 70.0 {
    palette.warning
  } else {
    palette.success
  }
}

/// Severity glyph for a temperature reading. Returns `""` on the
/// green tier (no glyph), `"△"` on yellow (warning), `"▲"` on red
/// (critical). Pairs with [`gpu_temp_color`] so themes that can't
/// carry colour information (Mono) still differentiate `92°C` from
/// `65°C` purely on glyph shape.
fn temp_severity_glyph(temp: f32) -> &'static str {
  if temp >= 82.0 {
    "▲"
  } else if temp >= 70.0 {
    "△"
  } else {
    ""
  }
}

/// Build the `glyph + value` spans for a temperature reading. The
/// glyph carries the severity tier so colour isn't load-bearing; both
/// glyph and value share the tier colour so colour-capable themes
/// double-encode the signal. Reserves zero cells on the green tier
/// (no glyph) so the common case still renders compactly.
fn temp_spans<'a>(temp: f32, palette: &'a Palette) -> Vec<Span<'a>> {
  let color = gpu_temp_color(temp, palette);
  let glyph = temp_severity_glyph(temp);
  let style = Style::default().fg(color);
  let mut spans: Vec<Span<'a>> = Vec::with_capacity(2);
  if !glyph.is_empty() {
    spans.push(Span::styled(glyph, style));
  }
  spans.push(Span::styled(format!("{temp:.0}°C"), style));
  spans
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tui::app::{App, AppOptions};
  use ratatui::backend::TestBackend;
  use ratatui::Terminal;

  fn render_lines(snap: HostMetricsSnapshot) -> Vec<String> {
    let app = App::new(AppOptions::default());
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(32, 7)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 32, 7), &snap, palette))
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
  fn gauge_tier_thresholds_match_plan() {
    let palette = crate::theme::palette_for(crate::theme::ThemeName::Macchiato);
    assert_eq!(gauge_color(0.0, palette), palette.success);
    assert_eq!(gauge_color(59.9, palette), palette.success);
    assert_eq!(gauge_color(60.0, palette), palette.warning);
    assert_eq!(gauge_color(84.9, palette), palette.warning);
    assert_eq!(gauge_color(85.0, palette), palette.error);
    assert_eq!(gauge_color(100.0, palette), palette.error);
  }

  #[test]
  fn gpu_temp_tier_thresholds_match_plan() {
    let palette = crate::theme::palette_for(crate::theme::ThemeName::Macchiato);
    assert_eq!(gpu_temp_color(50.0, palette), palette.success);
    assert_eq!(gpu_temp_color(70.0, palette), palette.warning);
    assert_eq!(gpu_temp_color(82.0, palette), palette.error);
  }

  #[test]
  fn temp_severity_glyph_tiers_match_color_tiers() {
    // No glyph on the green tier (compact), `△` on yellow, `▲` on red.
    assert_eq!(temp_severity_glyph(0.0), "");
    assert_eq!(temp_severity_glyph(69.9), "");
    assert_eq!(temp_severity_glyph(70.0), "△");
    assert_eq!(temp_severity_glyph(81.9), "△");
    assert_eq!(temp_severity_glyph(82.0), "▲");
    assert_eq!(temp_severity_glyph(105.0), "▲");
  }

  #[test]
  fn temp_glyph_double_encodes_severity_on_mono_palette() {
    // On Mono, `success` and `error` both collapse to `White` — colour
    // alone can't tell `92°C` apart from `65°C`. The leading severity
    // glyph carries the signal independently so the reading stays
    // legible without colour.
    let snap = HostMetricsSnapshot {
      cpu_pct: 30.0,
      cpu_temp_c: Some(92.0),
      ram_used_bytes: 1,
      ram_total_bytes: 100,
      gpu_backend: "cpu_only".into(),
      ..Default::default()
    };
    let app = {
      let mut a = App::new(AppOptions::default());
      a.options.theme = crate::theme::ThemeName::Mono;
      a
    };
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(40, 7)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 40, 7), &snap, palette))
      .unwrap();
    let buf = term.backend().buffer().clone();
    let mut frame = String::new();
    for y in 0..buf.area.height {
      for x in 0..buf.area.width {
        frame.push_str(buf.cell((x, y)).unwrap().symbol());
      }
      frame.push('\n');
    }
    assert!(
      frame.contains("▲92°C"),
      "critical CPU temp should carry the `▲` glyph on Mono: {frame}"
    );
  }

  #[test]
  fn cpu_only_omits_gpu_rows() {
    let snap = HostMetricsSnapshot {
      cpu_pct: 50.0,
      ram_used_bytes: 4 * 1024 * 1024 * 1024,
      ram_total_bytes: 16 * 1024 * 1024 * 1024,
      gpu_backend: "cpu_only".into(),
      ..Default::default()
    };
    let rows = render_lines(snap);
    let body = rows.join("\n");
    assert!(body.contains("CPU"));
    assert!(body.contains("MEM"));
    assert!(!body.contains("GPU"));
    assert!(!body.contains("VRAM"));
    assert!(
      rows
        .iter()
        .any(|r| r.contains("backend") && r.contains("CPU only")),
      "expected `backend  cpu only` row, got: {rows:#?}"
    );
  }

  #[test]
  fn apple_metal_collapses_gpu_to_unified_line() {
    let snap = HostMetricsSnapshot {
      cpu_pct: 30.0,
      ram_used_bytes: 8 * 1024 * 1024 * 1024,
      ram_total_bytes: 32 * 1024 * 1024 * 1024,
      gpu_backend: "apple_metal".into(),
      gpu_mem_total_bytes: Some(32 * 1024 * 1024 * 1024),
      gpu_device_count: 1,
      ..Default::default()
    };
    let rows = render_lines(snap);
    let body = rows.join("\n");
    assert!(body.contains("GPU"));
    assert!(
      rows
        .iter()
        .any(|r| r.contains("GPU") && r.contains("unified")),
      "GPU row should read `GPU  unified`, got: {rows:#?}"
    );
    assert!(!body.contains("VRAM"));
    assert!(rows.iter().any(|r| r.contains("Apple")));
  }

  #[test]
  fn nvidia_renders_all_four_gauges_plus_backend() {
    let snap = HostMetricsSnapshot {
      cpu_pct: 58.0,
      cpu_temp_c: Some(52.0),
      ram_used_bytes: 11 * 1024 * 1024 * 1024,
      ram_total_bytes: 32 * 1024 * 1024 * 1024,
      gpu_backend: "nvidia".into(),
      gpu_util_pct: Some(84.0),
      gpu_mem_used_bytes: Some(14 * 1024 * 1024 * 1024),
      gpu_mem_total_bytes: Some(24 * 1024 * 1024 * 1024),
      gpu_temp_c: Some(68.0),
      gpu_device_count: 1,
      ..Default::default()
    };
    let rows = render_lines(snap);
    let body = rows.join("\n");
    assert!(body.contains("CPU"));
    assert!(body.contains("MEM"));
    assert!(body.contains("GPU"));
    assert!(body.contains("VRAM"));
    assert!(body.contains("NVIDIA"));
    assert!(body.contains("1 GPU"));
  }

  #[test]
  fn multi_gpu_pluralizes_backend_label() {
    let snap = HostMetricsSnapshot {
      gpu_backend: "nvidia".into(),
      gpu_device_count: 2,
      gpu_mem_total_bytes: Some(48 * 1024 * 1024 * 1024),
      ..Default::default()
    };
    let rows = render_lines(snap);
    assert!(rows.iter().any(|r| r.contains("2 GPUs")));
  }

  #[test]
  fn ram_total_zero_renders_em_dash_value() {
    let snap = HostMetricsSnapshot {
      cpu_pct: 10.0,
      ram_used_bytes: 0,
      ram_total_bytes: 0,
      gpu_backend: "cpu_only".into(),
      ..Default::default()
    };
    let rows = render_lines(snap);
    let ram_row = rows.iter().find(|r| r.contains("MEM")).unwrap();
    assert!(
      ram_row.contains("—/—"),
      "expected `—/—` placeholder when total is 0, got: {ram_row:?}"
    );
  }

  #[test]
  fn cpu_over_100_pct_clamps_bar_but_keeps_label() {
    let snap = HostMetricsSnapshot {
      cpu_pct: 312.0,
      ram_used_bytes: 1,
      ram_total_bytes: 100,
      gpu_backend: "cpu_only".into(),
      ..Default::default()
    };
    let rows = render_lines(snap);
    let cpu_row = rows.iter().find(|r| r.contains("CPU")).unwrap();
    // Numeric label preserves the unclamped value so users see the
    // true multi-core sum.
    assert!(
      cpu_row.contains("312%"),
      "CPU row must keep the unclamped numeric label, got: {cpu_row:?}"
    );
  }

  #[test]
  fn ram_row_shows_full_total_and_star_on_unified_memory() {
    // Strix Halo / UMA APU: sysinfo sees the full 121 GiB pool. The RAM
    // row must show that true total (the same number `init` reports) —
    // NOT the total minus the shared/GTT portion. `sysinfo` already
    // counts the GPU's pool usage in `used`, so subtracting produced a
    // wrong value. The `RAM*` marker (from the `unified` flag) is what
    // tells the user the pool is shared with the GPU.
    const GIB: u64 = 1024 * 1024 * 1024;
    let snap = HostMetricsSnapshot {
      cpu_pct: 5.0,
      ram_used_bytes: 71 * GIB,
      ram_total_bytes: 121 * GIB,
      gpu_backend: "amd".into(),
      gpu_util_pct: Some(0.0),
      gpu_mem_used_bytes: Some(43 * GIB),
      gpu_mem_total_bytes: Some(65 * GIB),
      gpu_device_count: 1,
      uma_shared_total_bytes: Some(61 * GIB),
      uma_shared_used_bytes: Some(43 * GIB),
      unified: true,
      ..Default::default()
    };
    let rows = render_lines(snap);
    let ram_row = rows.iter().find(|r| r.contains("MEM")).unwrap();
    assert!(
      ram_row.contains("71/121G"),
      "RAM row must show the full sysinfo total, not minus UMA-shared, got: {ram_row:?}"
    );
    assert!(
      ram_row.contains("MEM*"),
      "RAM label should flag unified memory with `*`, got: {ram_row:?}"
    );
  }

  #[test]
  fn ram_row_keeps_raw_sysinfo_numbers_for_discrete_gpu() {
    // Discrete cards don't populate `uma_shared_*` — the RAM row must
    // pass sysinfo's numbers through untouched.
    const GIB: u64 = 1024 * 1024 * 1024;
    let snap = HostMetricsSnapshot {
      cpu_pct: 10.0,
      ram_used_bytes: 16 * GIB,
      ram_total_bytes: 64 * GIB,
      gpu_backend: "nvidia".into(),
      gpu_mem_used_bytes: Some(5 * GIB),
      gpu_mem_total_bytes: Some(24 * GIB),
      gpu_device_count: 1,
      ..Default::default()
    };
    let rows = render_lines(snap);
    let ram_row = rows.iter().find(|r| r.contains("MEM")).unwrap();
    assert!(ram_row.contains("16/64G"), "RAM row got: {ram_row:?}");
    assert!(
      !ram_row.contains("MEM*"),
      "discrete GPUs shouldn't carry the unified-memory star, got: {ram_row:?}"
    );
  }

  #[test]
  fn ram_and_vram_rows_render_unit_suffix_only_once() {
    // Regression: rows used to render `66G/121G` and `2.5G/4.0G` —
    // the `G` (or `M` / `K` / `B`) suffix appeared on both sides of
    // the slash. Pair formatter shares one trailing suffix.
    const GIB: u64 = 1024 * 1024 * 1024;
    let snap = HostMetricsSnapshot {
      cpu_pct: 10.0,
      ram_used_bytes: 66 * GIB,
      ram_total_bytes: 121 * GIB,
      gpu_backend: "nvidia".into(),
      gpu_util_pct: Some(40.0),
      gpu_mem_used_bytes: Some(2_642_341_888),
      gpu_mem_total_bytes: Some(4 * GIB),
      gpu_device_count: 1,
      ..Default::default()
    };
    let rows = render_lines(snap);
    let ram_row = rows.iter().find(|r| r.contains("MEM")).unwrap();
    let vram_row = rows.iter().find(|r| r.contains("VRAM")).unwrap();
    assert!(
      ram_row.contains("66/121G") && !ram_row.contains("66G/121G"),
      "RAM row should share one `G` suffix, got: {ram_row:?}"
    );
    assert!(
      vram_row.contains("2.5/4.0G") && !vram_row.contains("2.5G/4.0G"),
      "VRAM row should share one `G` suffix, got: {vram_row:?}"
    );
  }

  #[test]
  fn vram_gauge_uses_reachable_ceiling_on_unified_memory() {
    // UMA: the GPU shares one pool with the CPU, so the gauge
    // denominator is the reachable ceiling = pool − non-GPU RAM use,
    // NOT the static GTT cap and NOT the double-counting `pool − ram_used`.
    // pool 124, ram_used 71 (of which 43 is the GPU's GTT), so non-GPU
    // RAM = 28 and the GPU can reach 124 − 28 = 96.
    const GIB: u64 = 1024 * 1024 * 1024;
    let snap = HostMetricsSnapshot {
      cpu_pct: 5.0,
      ram_used_bytes: 71 * GIB,
      ram_total_bytes: 124 * GIB,
      gpu_backend: "amd".into(),
      gpu_util_pct: Some(10.0),
      gpu_mem_used_bytes: Some(43 * GIB),
      gpu_mem_total_bytes: Some(124 * GIB),
      gpu_device_count: 1,
      uma_shared_total_bytes: Some(123 * GIB),
      uma_shared_used_bytes: Some(43 * GIB),
      unified: true,
      ..Default::default()
    };
    let rows = render_lines(snap);
    let vram_row = rows.iter().find(|r| r.contains("VRAM")).unwrap();
    assert!(
      vram_row.contains("43/96G"),
      "UMA VRAM gauge should read used / reachable-ceiling, got: {vram_row:?}"
    );
    assert!(
      !vram_row.contains("43/53G"),
      "must not double-count the GPU's own bytes (pool − ram_used), got: {vram_row:?}"
    );
  }

  #[test]
  fn vram_gauge_uses_raw_total_on_discrete() {
    // Discrete cards keep `used / total` — RAM lives in a separate pool,
    // so subtracting it would be wrong.
    const GIB: u64 = 1024 * 1024 * 1024;
    let snap = HostMetricsSnapshot {
      cpu_pct: 5.0,
      ram_used_bytes: 50 * GIB,
      ram_total_bytes: 64 * GIB,
      gpu_backend: "nvidia".into(),
      gpu_util_pct: Some(10.0),
      gpu_mem_used_bytes: Some(14 * GIB),
      gpu_mem_total_bytes: Some(24 * GIB),
      gpu_device_count: 1,
      unified: false,
      ..Default::default()
    };
    let rows = render_lines(snap);
    let vram_row = rows.iter().find(|r| r.contains("VRAM")).unwrap();
    assert!(
      vram_row.contains("14/24G"),
      "discrete VRAM gauge stays used / total, got: {vram_row:?}"
    );
  }

  #[test]
  fn bar_width_scales_with_panel_width() {
    assert_eq!(bar_width_for(32), bar_width_for(32));
    assert!(bar_width_for(20) < bar_width_for(40));
    // Pathologically narrow panels still produce a minimum-width bar.
    assert!(bar_width_for(8) >= 4);
  }

  #[test]
  fn unsampled_backend_collapses_gpu_rows_like_cpu_only() {
    // The pre-first-tick window emits `gpu_backend == "unsampled"`. The
    // host panel must not render gpu_util_row/vram_row in that window
    // (they would show bars filled to 0% / "—/—" placeholders that
    // misrepresent the actual GPU state).
    let snap = HostMetricsSnapshot {
      cpu_pct: 10.0,
      ram_used_bytes: 1024 * 1024 * 1024,
      ram_total_bytes: 16 * 1024 * 1024 * 1024,
      gpu_backend: HostMetricsSnapshot::UNINITIALIZED_BACKEND.into(),
      ..Default::default()
    };
    let rows = render_lines(snap);
    let body = rows.join("\n");
    assert!(body.contains("CPU"));
    assert!(body.contains("MEM"));
    assert!(!body.contains("GPU"));
    assert!(!body.contains("VRAM"));
    assert!(
      rows.iter().any(|r| r.contains("detecting")),
      "expected `backend  unsampled`, got: {rows:#?}"
    );
  }

  #[test]
  fn empty_backend_collapses_gpu_rows_like_cpu_only() {
    // A default-constructed `HostMetricsSnapshot` (which the IPC
    // emits as `host` when no sampler is attached) carries
    // `gpu_backend == ""`. The pane should not render GPU/VRAM rows
    // for that state.
    let snap = HostMetricsSnapshot {
      gpu_backend: String::new(),
      ..Default::default()
    };
    let rows = render_lines(snap);
    let body = rows.join("\n");
    assert!(body.contains("CPU"));
    assert!(body.contains("MEM"));
    assert!(!body.contains("GPU"));
    assert!(!body.contains("VRAM"));
  }

  #[test]
  fn arbitrary_backend_string_falls_through_to_catch_all() {
    // An unrecognised label (e.g. a future backend not yet handled by
    // the explicit arms) should still render — just verbatim — so the
    // user gets a debuggable signal rather than a missing row.
    let snap = HostMetricsSnapshot {
      gpu_backend: "future-backend".into(),
      gpu_device_count: 1,
      gpu_mem_total_bytes: Some(8 * 1024 * 1024 * 1024),
      ..Default::default()
    };
    let rows = render_lines(snap);
    assert!(
      rows.iter().any(|r| r.contains("future-backend")),
      "expected verbatim catch-all label: {rows:#?}"
    );
  }

  #[test]
  fn unknown_backend_renders_via_constant_arm() {
    // The Vulkan fallback emits BACKEND_UNKNOWN. The backend row
    // should pick up the explicit "unknown" label rather than passing
    // through the catch-all (otherwise the wire string leaks into the
    // UI verbatim).
    let snap = HostMetricsSnapshot {
      gpu_backend: HostMetricsSnapshot::BACKEND_UNKNOWN.into(),
      ..Default::default()
    };
    let rows = render_lines(snap);
    assert!(
      rows.iter().any(|r| r.contains("unknown")),
      "expected `backend  unknown`, got: {rows:#?}"
    );
  }
}
