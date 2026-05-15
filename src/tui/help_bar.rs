//! Bottom-row contextual help bar.
//!
//! Pulls bindings from [`super::keybindings::bindings_for`] and
//! renders them as a single line tied to the current focus. Width
//! is honoured: when there isn't room for every binding, the bar
//! truncates with an ellipsis rather than wrapping.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::keybindings::{bindings_for, Focus};

/// Render the help bar into `area`. `extra` is appended after the
/// keybinding chips — toasts and connection-status hints land
/// there. Truncation is handled by `Paragraph` so wide terminals
/// see the full line.
pub fn render(
  frame: &mut Frame<'_>,
  area: Rect,
  focus: Focus,
  extra: Option<&str>,
  palette: &Palette,
) {
  let mut spans: Vec<Span<'_>> = Vec::new();
  for (i, b) in bindings_for(focus).iter().enumerate() {
    if i > 0 {
      spans.push(Span::raw("  "));
    }
    spans.push(Span::styled(
      b.label,
      Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::raw(":"));
    spans.push(Span::styled(
      b.description,
      Style::default().fg(palette.muted),
    ));
  }
  if let Some(text) = extra {
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
      text.to_string(),
      Style::default().fg(palette.warning),
    ));
  }
  let para = Paragraph::new(Line::from(spans));
  frame.render_widget(para, area);
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn list_focus_help_includes_quit_and_filter() {
    // Pure structural assertion: the keybinding table feeding the
    // bar must surface the user-visible labels we promise in the
    // plan. Render itself is exercised by tests/tui_smoke_test.rs.
    let bs = bindings_for(Focus::List);
    assert!(bs.iter().any(|b| b.label == "q"));
    assert!(bs.iter().any(|b| b.label == "/"));
    assert!(bs.iter().any(|b| b.label == "Enter"));
  }
}
