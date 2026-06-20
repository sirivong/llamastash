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

use crate::config::{KnobValue, KnobValueOpt};
use crate::launch::flag_aliases::{knob_display_groups, KnobField};
use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::keybindings::{Action, Focus};
use crate::tui::launch_picker::{LaunchPickerState, PickerField, INHERITED_LABEL};

/// Render the Settings tab body into `area`.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let mut lines: Vec<Line<'_>> = Vec::new();

  if app.launch_picker.is_none() {
    if let Some(m) = app.focused_managed() {
      lines.push(heading("Running launch", palette));
      lines.push(crate::tui::fmt::kv_row(
        "launch",
        m.launch_id.clone(),
        palette,
      ));
      // port / state / rss / cpu already render in the header info
      // row above the divider — dropping them here removes the
      // duplication that bloated the running-launch view.
      // A running launch shows what the server is *actually running*
      // with: the live dispatched knobs (`m.knobs`) — `auto` for a
      // fit-delegated row, a pinned number when set — not the user's
      // saved `last_params` delta (which is empty even for an auto
      // launch). `ctx` is overlaid with the real window `--fit` resolved
      // (read from `/props`); it's the one placement value llama-server
      // reports back, so every other row honestly stays `auto`.
      let dispatched = &m.knobs;
      let resolved_ctx = m.resolved_ctx.or(dispatched.ctx.set_value().copied());
      for group in knob_display_groups() {
        // Match the editable form: the whole Multi-GPU placement group
        // is hidden on single-GPU / CPU-only hosts.
        if group.multi_device_only && !app.multi_device() {
          continue;
        }
        lines.push(group_header(group.title, palette));
        for field in group.fields {
          let value = match field {
            KnobField::Ctx => resolved_ctx
              .map(|v| {
                // Flag a memory-driven clamp so the user knows the
                // window was squeezed to the floor, not chosen freely.
                if m.ctx_clamped {
                  format!("{v} · clamped to floor")
                } else {
                  v.to_string()
                }
              })
              .unwrap_or_else(|| format_persisted_knob_value(dispatched, KnobField::Ctx)),
            _ => format_persisted_knob_value(dispatched, *field),
          };
          lines.push(crate::tui::fmt::kv_row(field.field_name(), value, palette));
        }
      }
      let extras: String = app
        .last_params
        .get(&m.path)
        .map(|p| p.extras.join(" "))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(none)".into());
      lines.push(crate::tui::fmt::kv_row("extras", extras, palette));
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
      // Clamp scroll to the actual rendered height vs the viewport so
      // a window resize doesn't leave the view blanked. The stored
      // scroll is bumped freely by ↑/↓ event handlers — this is the
      // single point that ensures the rendered offset is in-bounds.
      // Write the clamped value back so over-scrolling past the
      // bottom doesn't inflate the stored offset (which would make a
      // subsequent ↑ press feel unresponsive until the offset
      // dropped back below `max_scroll`).
      //
      // Count *wrapped* rows, not logical lines: the trailing hint
      // ("Press `e` … re-launch.") wraps on a narrow pane, so a
      // logical-line clamp under-counts and leaves the tail unreachable
      // — cut off at the bottom edge.
      let para = Paragraph::new(lines).wrap(Wrap { trim: false });
      let max_scroll = (para.line_count(area.width) as u16).saturating_sub(area.height);
      let scroll = app.running_view_scroll.get().min(max_scroll);
      app.running_view_scroll.set(scroll);
      frame.render_widget(para.scroll((scroll, 0)), area);
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

  // Duplicate-launch heads-up. Surfaces at the top of the panel so
  // it remains visible even when the typed-knob list (12 rows) pushes
  // the launch-chip footer below the viewport.
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
  }

  // Show the right-aligned source chip once the pane has room for a
  // `label + value + (chip)` row. In wide mode the right pane is only
  // 35% of the terminal, so a 50-col gate kept the chip hidden until
  // ~150-col terminals; 40 surfaces it at realistic widths (a row fits
  // `2 marker + 16 label + value + "  (server default)"` by ~38 cols).
  let show_source = area.width >= 40;
  let row_for = |field: PickerField| picker_view.field == field;
  // Track the line index of the focused row so we can adjust the
  // scroll offset below — on tall viewports nothing scrolls; on
  // short ones the focused row stays visible with ≥1 row of context.
  let mut focused_line: Option<u16> = None;

  // Every typed knob — including ctx and reasoning — flows through
  // the same `value (chip)` shape, grouped by function with a header
  // per cluster (display order is distinct from argv order). Empty
  // rows render `inherited` as the value; the chip names the layer that
  // would supply it.
  for group in knob_display_groups() {
    // Skip the whole group — header included — when every row in it is
    // hidden (the Multi-GPU placement group on single-GPU / CPU-only
    // hosts, where each control can only ever hold `inherited`).
    if !group
      .fields
      .iter()
      .any(|f| picker_view.field_visible(PickerField::Knob(*f)))
    {
      continue;
    }
    lines.push(group_header(group.title, palette));
    for field in group.fields {
      let field = *field;
      if !picker_view.field_visible(PickerField::Knob(field)) {
        continue;
      }
      let focused = row_for(PickerField::Knob(field));
      if focused {
        focused_line = Some(lines.len() as u16);
      }
      if picker_view.inline_edit.is_open()
        && picker_view.inline_edit.field == Some(PickerField::Knob(field))
      {
        lines.push(inline_edit_row(
          field.field_name(),
          picker_view.inline_edit.input.buffer(),
          focused,
          palette,
        ));
        if let Some(err) = &picker_view.inline_edit.error {
          lines.push(inline_warning_row(err, palette));
        }
      } else {
        let value = format_knob_value(picker_view, field);
        let source = picker_view.source_for(field).label();
        lines.push(crate::tui::fmt::kv_row_focused(
          field.field_name(),
          value,
          Some(source),
          focused,
          true,
          palette,
          show_source,
        ));
      }
    }
  }

  // extras row
  let extras_focused = row_for(PickerField::Extras);
  if extras_focused {
    focused_line = Some(lines.len() as u16);
  }
  if picker_view.extras_input.is_editing() {
    lines.push(inline_edit_row(
      "extras",
      picker_view.extras_input.buffer(),
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
    lines.push(crate::tui::fmt::kv_row_focused(
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

  // Minimal-scroll-with-margin policy: keep ≥1 row of context above
  // and below the focused row so the user sees what's adjacent. When
  // the focused row crosses an edge, scroll just enough to restore
  // the margin — no jumping to top/bottom, no centring. Recomputed
  // every render so window resizes self-correct.
  let scroll = clamp_scroll_with_margin(
    picker_view.scroll_offset.get(),
    focused_line.unwrap_or(0),
    area.height,
    lines.len() as u16,
  );
  picker_view.scroll_offset.set(scroll);

  frame.render_widget(
    Paragraph::new(lines)
      .scroll((scroll, 0))
      .wrap(Wrap { trim: false }),
    area,
  );
}

/// Minimal scroll with margin: keep the focused row visible with
/// `MARGIN` rows of context above and below where possible. Returns
/// the new scroll offset. Clamped to `[0, max_scroll]`.
fn clamp_scroll_with_margin(current: u16, focused: u16, viewport: u16, total: u16) -> u16 {
  const MARGIN: u16 = 1;
  let max_scroll = total.saturating_sub(viewport);
  if viewport == 0 {
    return 0;
  }
  // Scroll up so focused is at least MARGIN rows below the top.
  let upper_bound = focused.saturating_sub(MARGIN);
  // Scroll down so focused is at least MARGIN rows above the bottom.
  let lower_bound = focused.saturating_add(MARGIN + 1).saturating_sub(viewport);
  let mut next = current;
  if next > upper_bound {
    next = upper_bound;
  }
  if next < lower_bound {
    next = lower_bound;
  }
  next.min(max_scroll)
}

/// Read-only formatter for the running-launch view. Same vocabulary
/// as `format_knob_value` (value or `inherited` / `on` / `off`) but
/// reads straight from a persisted `TypedKnobs` instead of a picker
/// state. Untouched fields render `inherited` — the user can open the
/// editor (`e`) to see the resolved chip.
/// Render a persisted knob slot: a pinned value verbatim, the literal
/// `auto` for the Auto state, and `inherited` for an unset (Inherited)
/// slot. Empty strings (cleared `device` / `tensor_split` / `split_mode`)
/// read as `inherited` too.
fn knob_value_label<T: std::fmt::Display>(slot: &Option<KnobValue<T>>) -> String {
  match slot {
    Some(KnobValue::Set(v)) => {
      let s = v.to_string();
      if s.is_empty() {
        INHERITED_LABEL.into()
      } else {
        s
      }
    }
    Some(KnobValue::Auto) => "auto".into(),
    None => INHERITED_LABEL.into(),
  }
}

fn format_persisted_knob_value(knobs: &crate::config::TypedKnobs, field: KnobField) -> String {
  match field {
    KnobField::Ctx => knob_value_label(&knobs.ctx),
    KnobField::NGpuLayers => knob_value_label(&knobs.n_gpu_layers),
    KnobField::NCpuMoe => knob_value_label(&knobs.n_cpu_moe),
    KnobField::Threads => knob_value_label(&knobs.threads),
    KnobField::Parallel => knob_value_label(&knobs.parallel),
    KnobField::BatchSize => knob_value_label(&knobs.batch_size),
    KnobField::UbatchSize => knob_value_label(&knobs.ubatch_size),
    KnobField::Keep => knob_value_label(&knobs.keep),
    KnobField::RopeFreqScale => knob_value_label(&knobs.rope_freq_scale),
    KnobField::CacheTypeK => knob_value_label(&knobs.cache_type_k),
    KnobField::CacheTypeV => knob_value_label(&knobs.cache_type_v),
    KnobField::Reasoning => bool_label(&knobs.reasoning),
    KnobField::FlashAttn => bool_label(&knobs.flash_attn),
    KnobField::Mlock => bool_label(&knobs.mlock),
    KnobField::NoMmap => bool_label(&knobs.no_mmap),
    KnobField::Device => knob_value_label(&knobs.device),
    KnobField::MainGpu => knob_value_label(&knobs.main_gpu),
    KnobField::TensorSplit => knob_value_label(&knobs.tensor_split),
    KnobField::SplitMode => knob_value_label(&knobs.split_mode),
  }
}

fn bool_label(v: &Option<KnobValue<bool>>) -> String {
  match v.set_value().copied() {
    Some(true) => "on".into(),
    Some(false) => "off".into(),
    None if v.is_auto() => "auto".into(),
    None => INHERITED_LABEL.into(),
  }
}

fn format_knob_value(state: &LaunchPickerState, field: KnobField) -> String {
  // The Auto stop renders as `auto` regardless of value kind — fit
  // governs the knob, so there is no concrete value to show.
  if state.effective_is_auto(field) {
    return "auto".into();
  }
  match field {
    KnobField::Ctx
    | KnobField::NGpuLayers
    | KnobField::NCpuMoe
    | KnobField::Threads
    | KnobField::Parallel
    | KnobField::BatchSize
    | KnobField::UbatchSize
    | KnobField::Keep
    | KnobField::MainGpu => state
      .effective_u32(field)
      .map(|v| v.to_string())
      .unwrap_or_else(|| INHERITED_LABEL.into()),
    KnobField::RopeFreqScale => state
      .effective_f32(field)
      .map(|v| format!("{v}"))
      .unwrap_or_else(|| INHERITED_LABEL.into()),
    KnobField::CacheTypeK
    | KnobField::CacheTypeV
    | KnobField::TensorSplit
    | KnobField::SplitMode => state
      .effective_str(field)
      .unwrap_or_else(|| INHERITED_LABEL.into()),
    KnobField::Device => state.device_value_display(),
    KnobField::Reasoning | KnobField::FlashAttn | KnobField::Mlock | KnobField::NoMmap => {
      match state.effective_bool(field) {
        Some(true) => "on".into(),
        Some(false) => "off".into(),
        None => INHERITED_LABEL.into(),
      }
    }
  }
}

fn heading<'a>(text: &'a str, palette: &Palette) -> Line<'a> {
  Line::from(Span::styled(
    text,
    Style::default()
      .fg(palette.highlight)
      .add_modifier(Modifier::BOLD),
  ))
}

/// Quiet divider above each knob cluster — `── Title`, indented to
/// align with the rows below it and painted in the muted tone so it
/// reads as a separator, not a value row.
fn group_header(title: &str, palette: &Palette) -> Line<'static> {
  Line::from(Span::styled(
    format!(
      "  {}{} {title}",
      crate::tui::glyphs::active().hline(),
      crate::tui::glyphs::active().hline()
    ),
    palette.muted_style().add_modifier(Modifier::BOLD),
  ))
}

const LABEL_W: usize = 16;

fn chip_label(chip: &str) -> &str {
  chip.split(':').next().unwrap_or(chip)
}

fn inline_edit_row(label: &str, buffer: &str, focused: bool, palette: &Palette) -> Line<'static> {
  let marker = if focused {
    crate::tui::glyphs::active().focus_marker()
  } else {
    "  "
  };
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

  #[test]
  fn clamp_scroll_keeps_focused_visible_with_margin() {
    // Focused row inside the viewport — no change.
    assert_eq!(clamp_scroll_with_margin(0, 5, 20, 30), 0);
    // Focused below the viewport bottom — scroll just enough to land
    // with one row of margin below.
    assert_eq!(clamp_scroll_with_margin(0, 19, 10, 30), 11);
    // Focused above the viewport top — scroll up to land one row
    // below the top edge.
    assert_eq!(clamp_scroll_with_margin(15, 5, 10, 30), 4);
    // Focused at index 0 with no margin available — saturate at 0.
    assert_eq!(clamp_scroll_with_margin(5, 0, 10, 30), 0);
    // Viewport bigger than content — never scroll.
    assert_eq!(clamp_scroll_with_margin(0, 5, 50, 10), 0);
    // Zero viewport returns 0 (would otherwise underflow).
    assert_eq!(clamp_scroll_with_margin(5, 5, 0, 30), 0);
  }

  fn fake_model(path: &str, parent: &str) -> crate::discovery::DiscoveredModel {
    crate::discovery::DiscoveredModel {
      path: PathBuf::from(path),
      parent: PathBuf::from(parent),
      source: crate::discovery::ModelSource::UserPath,
      metadata: None,
      parse_error: None,
      split_siblings: Vec::new(),
      display_label: None,
      multimodal: None,
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
        // ctx/reasoning now live inside `knobs`; the picker seeds
        // `user_knobs` straight from `knobs` so a returning user
        // sees their last-shipped values with `(user)` chips.
        knobs: crate::config::TypedKnobs {
          ctx: Some(KnobValue::Set(16384)),
          reasoning: Some(KnobValue::Set(true)),
          ..Default::default()
        },
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
  fn source_chip_shows_at_40_cols_hidden_at_39() {
    use crate::tui::app::LastParamsRow;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;

    // A user-set ctx earns a `(user)` source chip on its row, which is
    // the marker we assert on. The right pane in wide mode is only ~35%
    // of the terminal, so the chip gate must trip well below 50 cols.
    let render_at = |w: u16| -> String {
      let mut app = App::new(AppOptions::default());
      let path = PathBuf::from("/m/qwen.gguf");
      app.models = vec![fake_model("/m/qwen.gguf", "/m")];
      app.last_params.insert(
        path,
        LastParamsRow {
          ctx: Some(16384),
          reasoning: false,
          knobs: crate::config::TypedKnobs {
            ctx: Some(KnobValue::Set(16384)),
            ..Default::default()
          },
          extras: vec![],
          port: Some(41100),
        },
      );
      app.list_cursor = 2;
      let palette = app.palette();
      let mut term = Terminal::new(TestBackend::new(w, 32)).unwrap();
      term
        .draw(|f| render(f, Rect::new(0, 0, w, 32), &app, palette))
        .unwrap();
      let buf = term.backend().buffer().clone();
      let mut joined = String::new();
      for y in 0..buf.area.height {
        for x in 0..buf.area.width {
          joined.push_str(buf.cell((x, y)).unwrap().symbol());
        }
        joined.push('\n');
      }
      joined
    };

    let wide = render_at(40);
    assert!(
      wide.contains("(user)"),
      "source chip must show at 40 cols: {wide}"
    );
    let narrow = render_at(39);
    assert!(
      !narrow.contains("(user)"),
      "source chip must be hidden below 40 cols: {narrow}"
    );
  }

  #[test]
  fn running_view_shows_resolved_ctx_and_dispatched_auto_knobs() {
    use crate::config::{KnobValue, TypedKnobs};
    use crate::tui::app::ManagedRow;
    use crate::tui::status_icons::SurfaceState;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;
    let mut app = App::new(AppOptions::default());
    let path = PathBuf::from("/m/gemma.gguf");
    app.models = vec![fake_model("/m/gemma.gguf", "/m")];
    // The managed row carries the *dispatched* knobs (what the server is
    // running with) — all `auto` for an all-auto launch — plus the ctx
    // `--fit` actually resolved, read from `/props`. `last_params` is
    // deliberately left empty to prove the running view reads the live
    // dispatch, not the saved delta.
    app.managed = vec![ManagedRow {
      launch_id: "L1".into(),
      path: path.clone(),
      port: 41101,
      state: SurfaceState::Ready,
      resolved_ctx: Some(262144),
      knobs: TypedKnobs {
        ctx: Some(KnobValue::Auto),
        parallel: Some(KnobValue::Auto),
        threads: Some(KnobValue::Auto),
        ..Default::default()
      },
      ..Default::default()
    }];
    // Row 0 header, row 1 `▶ Running`, row 2 the running launch.
    app.list_cursor = 2;
    assert!(app.launch_picker.is_none());
    assert!(
      app.focused_managed().is_some(),
      "cursor must land on the launch"
    );
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(70, 40)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 70, 40), &app, palette))
      .unwrap();
    let buf = term.backend().buffer().clone();
    let mut joined = String::new();
    for y in 0..buf.area.height {
      for x in 0..buf.area.width {
        joined.push_str(buf.cell((x, y)).unwrap().symbol());
      }
      joined.push('\n');
    }
    // ctx shows the resolved number, NOT `default` and NOT `auto`.
    assert!(joined.contains("262144"), "resolved ctx missing: {joined}");
    // A fit-delegated knob reads `auto` (from the dispatched knobs),
    // never `default` — even though `last_params` is empty.
    let threads_line = joined.lines().find(|l| l.contains("threads")).unwrap_or("");
    assert!(
      threads_line.contains("auto"),
      "dispatched auto knob should read auto, not default: {threads_line:?}"
    );
    let parallel_line = joined
      .lines()
      .find(|l| l.contains("parallel"))
      .unwrap_or("");
    assert!(
      parallel_line.contains("auto"),
      "parallel is fit-delegated and not read back, so it reads auto: {parallel_line:?}"
    );
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
    use crate::tui::keybindings::ENTER_LABEL;
    let expected = format!("{ENTER_LABEL} again to launch with these settings.");
    assert!(joined.contains(&expected), "{joined}");
  }
}
