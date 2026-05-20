//! Settings tab — typed-knob launch editor for the focused model.
//!
//! Renders a vertical list of rows: `ctx`, `reasoning`, every
//! `TypedKnobs` field with a per-row source label, and an `extras`
//! free-text row at the bottom. When the focused model has a
//! running launch and the picker isn't open, shows the live params
//! (read-only).

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::launch::flag_aliases::{knob_specs, KnobField};
use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::keybindings::{Action, Focus};
use crate::tui::launch_picker::{LaunchPickerState, PickerField};

/// Render the Settings tab body into `area`.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let mut lines: Vec<Line<'_>> = Vec::new();

  if app.launch_picker.is_none() {
    if let Some(m) = app.focused_managed() {
      lines.push(heading("Running launch", palette));
      lines.push(kv("launch", m.launch_id.clone(), palette));
      lines.push(kv("port", format!(":{}", m.port), palette));
      lines.push(kv(
        "state",
        crate::tui::status_icons::label_for(m.state).to_string(),
        palette,
      ));
      if let Some(rss) = m.rss_bytes {
        lines.push(kv("rss", crate::tui::fmt::format_bytes(rss), palette));
      }
      if let Some(cpu) = m.cpu_pct {
        lines.push(kv("cpu", format!("{cpu:.0}%"), palette));
      }
      let last = app.last_params.get(&m.path);
      lines.push(Line::default());
      lines.push(heading("Launch params", palette));
      let ctx_value = last
        .and_then(|p| p.ctx)
        .map(|c| c.to_string())
        .unwrap_or_else(|| "native".into());
      lines.push(kv("ctx", ctx_value, palette));
      let reasoning_value = last.map(|p| p.reasoning).unwrap_or(false);
      lines.push(kv(
        "reasoning",
        if reasoning_value { "on" } else { "off" }.into(),
        palette,
      ));
      let extras: String = last
        .map(|p| p.extras.join(" "))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(none)".into());
      lines.push(kv("extras", extras, palette));
      lines.push(Line::default());
      let edit_chip = app
        .hint_with(Focus::RightPane, Action::EnterEdit, "edit for launch")
        .map(|c| chip_label(&c).to_string())
        .unwrap_or_else(|| "e".to_string());
      lines.push(
        Span::styled(
          format!("Press `{edit_chip}` to edit next-launch params, or `s` to stop and re-launch."),
          palette.muted_style(),
        )
        .into(),
      );
      frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
      return;
    }
  }

  // Editable form.
  let default_picker: LaunchPickerState;
  let picker_view: &LaunchPickerState = match app.launch_picker.as_ref() {
    Some(p) => p,
    None => {
      default_picker = app.build_default_picker().unwrap_or_else(|| {
        let name = app.focused_name().unwrap_or_else(|| "(none)".into());
        LaunchPickerState::for_model(name)
      });
      &default_picker
    }
  };
  let no_focus = app.focused_path().is_none();

  lines.push(heading(
    if no_focus {
      "No model focused"
    } else {
      "Launch settings"
    },
    palette,
  ));
  lines.push(kv("model", picker_view.model_name.clone(), palette));

  let show_source = area.width >= 50;
  let row_for = |field: PickerField| picker_view.field == field;

  // ctx row
  let ctx_value = match picker_view.ctx {
    Some(n) => format!("{n}"),
    None => "native (GGUF default)".to_string(),
  };
  lines.push(kv_focused(
    "ctx",
    ctx_value,
    None,
    row_for(PickerField::Ctx),
    true,
    palette,
    show_source,
  ));
  // reasoning row
  lines.push(kv_focused(
    "reasoning",
    picker_view.reasoning.label().to_string(),
    None,
    row_for(PickerField::Reasoning),
    true,
    palette,
    show_source,
  ));

  // typed knob rows
  for spec in knob_specs() {
    let field = spec.field;
    let focused = row_for(PickerField::Knob(field));
    if picker_view.inline_edit.is_open()
      && picker_view.inline_edit.field == Some(PickerField::Knob(field))
    {
      lines.push(inline_edit_row(
        knob_label(field),
        &picker_view.inline_edit.buffer,
        focused,
        palette,
      ));
      if let Some(err) = &picker_view.inline_edit.error {
        lines.push(inline_warning_row(err, palette));
      }
    } else {
      let value = format_knob_value(picker_view, field);
      let source = picker_view.source_for(field).label();
      lines.push(kv_focused(
        knob_label(field),
        value,
        Some(source),
        focused,
        true,
        palette,
        show_source,
      ));
    }
  }

  // extras row
  let extras_focused = row_for(PickerField::Extras);
  if picker_view.extras_editing {
    lines.push(inline_edit_row(
      "extras",
      &picker_view.extras_buffer,
      extras_focused,
      palette,
    ));
  } else {
    let extras_text = if picker_view.extras.is_empty() {
      "(none)".to_string()
    } else {
      picker_view
        .extras
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
    };
    lines.push(kv_focused(
      "extras",
      extras_text,
      None,
      extras_focused,
      false,
      palette,
      show_source,
    ));
  }

  // Forbidden-flag warning under extras row.
  let banned = crate::launch::params::forbidden_in_extras(&picker_view.extras);
  if !banned.is_empty() {
    let redacted = crate::launch::params::redact_for_display(&picker_view.extras);
    lines.push(inline_warning_row(
      &format!("forbidden: {redacted}"),
      palette,
    ));
  }

  lines.push(Line::default());
  if picker_view.active_instances > 0 {
    lines.push(
      Span::styled(
        format!(
          "⚠ {n} instance{plural} already running — Enter launches a new one on a fresh port",
          n = picker_view.active_instances,
          plural = if picker_view.active_instances == 1 {
            ""
          } else {
            "s"
          }
        ),
        Style::default()
          .fg(palette.warning)
          .add_modifier(Modifier::BOLD),
      )
      .into(),
    );
    lines.push(Line::default());
  }
  let launch_chip = app
    .hint_with(Focus::RightPane, Action::Submit, "launch")
    .map(|chip| {
      format!(
        "Press {} again to launch with these settings.",
        chip_label(&chip)
      )
    })
    .unwrap_or_else(|| "Launch binding removed — set `submit` in config.".to_string());
  lines.push(
    Span::styled(
      if no_focus {
        "Select a model in the list to configure launch settings.".to_string()
      } else {
        launch_chip
      },
      palette.muted_style(),
    )
    .into(),
  );

  frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn knob_label(field: KnobField) -> &'static str {
  match field {
    KnobField::NGpuLayers => "n_gpu_layers",
    KnobField::Threads => "threads",
    KnobField::CacheTypeK => "cache_type_k",
    KnobField::CacheTypeV => "cache_type_v",
    KnobField::FlashAttn => "flash_attn",
    KnobField::Mlock => "mlock",
    KnobField::NoMmap => "no_mmap",
    KnobField::Parallel => "parallel",
    KnobField::BatchSize => "batch_size",
    KnobField::UbatchSize => "ubatch_size",
    KnobField::RopeFreqScale => "rope_freq_scale",
    KnobField::Keep => "keep",
  }
}

fn format_knob_value(state: &LaunchPickerState, field: KnobField) -> String {
  match field {
    KnobField::NGpuLayers
    | KnobField::Threads
    | KnobField::Parallel
    | KnobField::BatchSize
    | KnobField::UbatchSize
    | KnobField::Keep => state
      .effective_u32(field)
      .map(|v| v.to_string())
      .unwrap_or_else(|| "default".into()),
    KnobField::RopeFreqScale => state
      .effective_f32(field)
      .map(|v| format!("{v}"))
      .unwrap_or_else(|| "default".into()),
    KnobField::CacheTypeK | KnobField::CacheTypeV => state
      .effective_str(field)
      .unwrap_or_else(|| "default".into()),
    KnobField::FlashAttn | KnobField::Mlock | KnobField::NoMmap => {
      match state.effective_bool(field) {
        Some(true) => "on".into(),
        Some(false) => "off".into(),
        None => "default".into(),
      }
    }
  }
}

fn heading<'a>(text: &'a str, palette: &Palette) -> Line<'a> {
  Line::from(Span::styled(
    text,
    Style::default()
      .fg(palette.accent)
      .add_modifier(Modifier::BOLD),
  ))
}

const LABEL_W: usize = 16;

fn kv(label: &str, value: String, palette: &Palette) -> Line<'static> {
  Line::from(vec![
    Span::styled(
      format!("  {label:<width$}", width = LABEL_W),
      palette.muted_style(),
    ),
    Span::styled(value, palette.text_style()),
  ])
}

fn chip_label(chip: &str) -> &str {
  chip.split(':').next().unwrap_or(chip)
}

/// Editable form row. When focused and `cyclable`, the value is
/// wrapped in `◀ … ▶` so the user sees that Left/Right change it.
/// When `source_label` is `Some` and `show_source` is true, a
/// right-aligned `(<label>)` chip is appended.
#[allow(clippy::too_many_arguments)]
fn kv_focused(
  label: &str,
  value: String,
  source_label: Option<&str>,
  focused: bool,
  cyclable: bool,
  palette: &Palette,
  show_source: bool,
) -> Line<'static> {
  let marker = if focused { "→ " } else { "  " };
  let label_style = if focused {
    Style::default()
      .fg(palette.accent)
      .add_modifier(Modifier::BOLD)
  } else {
    palette.muted_style()
  };
  let mut spans: Vec<Span<'static>> = Vec::with_capacity(6);
  spans.push(Span::styled(
    format!("{marker}{label:<width$}", width = LABEL_W),
    label_style,
  ));
  if focused && cyclable {
    spans.push(Span::styled("◀ ".to_string(), palette.accent_style()));
    spans.push(Span::styled(value, palette.text_style()));
    spans.push(Span::styled(" ▶".to_string(), palette.accent_style()));
  } else {
    spans.push(Span::styled(value, palette.text_style()));
  }
  if let (true, Some(src)) = (show_source, source_label) {
    spans.push(Span::styled(format!("  ({src})"), palette.muted_style()));
  }
  Line::from(spans)
}

fn inline_edit_row(label: &str, buffer: &str, focused: bool, palette: &Palette) -> Line<'static> {
  let marker = if focused { "→ " } else { "  " };
  let label_style = Style::default()
    .fg(palette.accent)
    .add_modifier(Modifier::BOLD);
  Line::from(vec![
    Span::styled(
      format!("{marker}{label:<width$}", width = LABEL_W),
      label_style,
    ),
    Span::styled("[ ".to_string(), palette.muted_style()),
    Span::styled(buffer.to_string(), palette.text_style()),
    crate::tui::fmt::caret(palette),
    Span::styled(" ]".to_string(), palette.muted_style()),
  ])
}

fn inline_warning_row(message: &str, palette: &Palette) -> Line<'static> {
  Line::from(Span::styled(
    format!("    ⚠ {message}"),
    Style::default()
      .fg(palette.warning)
      .add_modifier(Modifier::BOLD),
  ))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tui::app::{App, AppOptions};
  use std::path::PathBuf;

  fn fake_model(path: &str, parent: &str) -> crate::discovery::DiscoveredModel {
    crate::discovery::DiscoveredModel {
      path: PathBuf::from(path),
      parent: PathBuf::from(parent),
      source: crate::discovery::ModelSource::UserPath,
      metadata: None,
      parse_error: None,
      split_siblings: Vec::new(),
    }
  }

  #[test]
  fn settings_form_reflects_last_params_on_first_render() {
    use crate::tui::app::LastParamsRow;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;
    let mut app = App::new(AppOptions::default());
    let path = PathBuf::from("/m/qwen.gguf");
    app.models = vec![fake_model("/m/qwen.gguf", "/m")];
    app.last_params.insert(
      path.clone(),
      LastParamsRow {
        ctx: Some(16384),
        reasoning: true,
        knobs: Default::default(),
        extras: vec!["--rope-freq-base".into(), "10000".into()],
        port: Some(41100),
      },
    );
    app.list_cursor = 2;
    assert!(app.launch_picker.is_none());
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(60, 32)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 60, 32), &app, palette))
      .unwrap();
    let buf = term.backend().buffer().clone();
    let mut joined = String::new();
    for y in 0..buf.area.height {
      for x in 0..buf.area.width {
        joined.push_str(buf.cell((x, y)).unwrap().symbol());
      }
      joined.push('\n');
    }
    assert!(joined.contains("16384"), "{joined}");
    assert!(joined.contains("on"), "{joined}");
  }

  #[test]
  fn launch_hint_reads_press_enter_again_to_launch() {
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model("/m/qwen.gguf", "/m")];
    app.list_cursor = 2;
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(70, 36)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 70, 36), &app, palette))
      .unwrap();
    let buf = term.backend().buffer().clone();
    let mut joined = String::new();
    for y in 0..buf.area.height {
      for x in 0..buf.area.width {
        joined.push_str(buf.cell((x, y)).unwrap().symbol());
      }
      joined.push('\n');
    }
    assert!(
      joined.contains("Enter again to launch with these settings."),
      "{joined}"
    );
  }
}
