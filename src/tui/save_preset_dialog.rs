//! `Ctrl+P` "save current launch settings as a preset" dialog.
//!
//! A small two-stage modal that captures the launch settings in view (the
//! Settings-tab form's user knobs, or a running model's live knobs) and
//! saves them to `config.yaml` as a named preset via `presets_save`.
//! Stage `Name` prompts for the preset name; if that name already exists
//! for the model, stage `Overwrite` asks to confirm the replacement.
//!
//! Auto / inherited markers ride through untouched: the captured
//! [`crate::config::TypedKnobs`] keeps each knob's `Set` / `Auto` /
//! inherited state, so a `ctx: auto` saved here round-trips as `auto`.

use std::path::PathBuf;

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::config::TypedKnobs;
use crate::theme::Palette;
use crate::tui::input_field::InputField;

/// Which step of the save flow is in view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveStage {
  /// Typing the preset name.
  Name,
  /// The typed name already exists — confirm the overwrite.
  Overwrite,
}

/// State for the `Ctrl+P` save-preset modal.
#[derive(Debug, Clone)]
pub struct SavePresetDialog {
  /// Canonical path of the model the preset is saved for.
  pub model_path: PathBuf,
  /// Display name shown in the dialog title.
  pub model_name: String,
  /// Captured launch knobs (ctx/reasoning folded in; Auto preserved).
  pub knobs: TypedKnobs,
  /// Captured extras argv tail.
  pub extras: Vec<String>,
  /// Names of the model's existing effective presets (overwrite check).
  pub existing: Vec<String>,
  /// The name-entry field.
  pub input: InputField,
  pub stage: SaveStage,
  /// Inline validation error (empty name), rendered under the input.
  pub error: Option<String>,
}

impl SavePresetDialog {
  /// Open the dialog at the name stage with the input ready for typing.
  pub fn open(
    model_path: PathBuf,
    model_name: String,
    knobs: TypedKnobs,
    extras: Vec<String>,
    existing: Vec<String>,
  ) -> Self {
    let mut input = InputField::new();
    input.enter_edit();
    Self {
      model_path,
      model_name,
      knobs,
      extras,
      existing,
      input,
      stage: SaveStage::Name,
      error: None,
    }
  }

  /// The trimmed preset name as typed.
  pub fn name(&self) -> String {
    self.input.buffer().trim().to_string()
  }

  /// Whether the typed name already names one of the model's presets.
  pub fn name_exists(&self) -> bool {
    self.existing.contains(&self.name())
  }
}

/// Render the modal. Caller invokes this only when the dialog is open.
pub fn render(frame: &mut Frame<'_>, area: Rect, dialog: &SavePresetDialog, palette: &Palette) {
  let rect = crate::tui::layout::centered_abs(area, 60, 9, 4, 2);
  frame.render_widget(Clear, rect);
  crate::tui::render::paint_theme_bg(frame, rect, palette);

  let tone = palette.accent;
  let block = palette
    .panel()
    .title(Line::from(Span::styled(
      format!(" Save preset · {} ", dialog.model_name),
      Style::default().fg(tone).add_modifier(Modifier::BOLD),
    )))
    .border(tone)
    .build();
  let inner = block.inner(rect);
  frame.render_widget(block, rect);

  let chunks = Layout::default()
    .direction(Direction::Vertical)
    .constraints([
      Constraint::Length(1), // prompt
      Constraint::Length(1), // input / confirm
      Constraint::Length(1), // error / spacer
      Constraint::Min(1),    // hint
    ])
    .split(inner);

  match dialog.stage {
    SaveStage::Name => {
      let prompt = Paragraph::new(Line::from(Span::styled(
        "Name this preset:",
        palette.text_style(),
      )));
      frame.render_widget(prompt, chunks[0]);

      let input_line = Paragraph::new(Line::from(vec![
        Span::styled("› ", Style::default().fg(tone)),
        Span::styled(dialog.input.buffer(), palette.text_style()),
        Span::styled("▏", Style::default().fg(tone)),
      ]));
      frame.render_widget(input_line, chunks[1]);

      if let Some(err) = &dialog.error {
        let e = Paragraph::new(Line::from(Span::styled(
          err.clone(),
          Style::default().fg(palette.error),
        )));
        frame.render_widget(e, chunks[2]);
      }

      let hint = Paragraph::new(Line::from(vec![
        Span::styled("Enter", Style::default().fg(palette.success)),
        Span::styled(" save  ·  ", palette.muted_style()),
        Span::styled("Esc", Style::default().fg(palette.warning)),
        Span::styled(" cancel", palette.muted_style()),
      ]))
      .alignment(Alignment::Center);
      frame.render_widget(hint, chunks[3]);
    }
    SaveStage::Overwrite => {
      let body = Paragraph::new(Line::from(Span::styled(
        format!(
          "A preset named `{}` already exists for {}. Overwrite it?",
          dialog.name(),
          dialog.model_name
        ),
        palette.text_style(),
      )))
      .wrap(Wrap { trim: true })
      .alignment(Alignment::Center);
      // Span the three top rows for the wrapped question.
      let body_area = Rect {
        height: chunks[0].height + chunks[1].height + chunks[2].height,
        ..chunks[0]
      };
      frame.render_widget(body, body_area);

      let hint = Paragraph::new(Line::from(vec![
        Span::styled("Enter / y", Style::default().fg(palette.success)),
        Span::styled(" overwrite  ·  ", palette.muted_style()),
        Span::styled("Esc / n", Style::default().fg(palette.warning)),
        Span::styled(" keep", palette.muted_style()),
      ]))
      .alignment(Alignment::Center);
      frame.render_widget(hint, chunks[3]);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::PathBuf;

  fn dialog(existing: &[&str]) -> SavePresetDialog {
    SavePresetDialog::open(
      PathBuf::from("/m/a.gguf"),
      "a.gguf".into(),
      TypedKnobs::default(),
      Vec::new(),
      existing.iter().map(|s| s.to_string()).collect(),
    )
  }

  #[test]
  fn opens_at_name_stage_with_editing_input() {
    let d = dialog(&[]);
    assert_eq!(d.stage, SaveStage::Name);
    assert!(d.input.is_editing());
    assert!(d.error.is_none());
  }

  #[test]
  fn name_is_trimmed_and_existence_checked() {
    let mut d = dialog(&["coding", "long-ctx"]);
    d.input.set_text("  coding  ");
    assert_eq!(d.name(), "coding");
    assert!(d.name_exists());
    d.input.set_text("fresh");
    assert!(!d.name_exists());
  }
}
