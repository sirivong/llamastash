//! Modal `?` help overlay listing every keybinding grouped by
//! category. Layout is fully derived from [`DEFAULT_BINDINGS`] —
//! each binding row carries its own `categories` list, and the
//! renderer walks every [`Category`] in order, collecting rows
//! whose binding lands in that section.
//!
//! Bindings come from `App::bindings_for` so config overrides flow
//! through automatically.

use std::collections::BTreeMap;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};
use ratatui::Frame;

use crate::theme::Palette;
use crate::tui::app::App;
use crate::tui::keybindings::{Action, Binding, Category, Focus};

/// Render the overlay. Caller is responsible for only invoking
/// this when `app.show_help` is true. Layout is static — the active
/// focus does not change which sections appear or where.
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let rect = centred(
    area,
    area.width.saturating_sub(4).min(130),
    area.height.saturating_sub(4).max(20),
  );
  frame.render_widget(Clear, rect);
  crate::tui::render::paint_theme_bg(frame, rect, palette);

  // Close chip carries both keys that dismiss the overlay:
  //   - `Esc` is hardcoded modal-dismiss in `events::handle_key`
  //     (not an Action), so it always works regardless of the
  //     `toggle_help` binding; we hardcode it here too.
  //   - The `toggle_help` action's live key (default `?`) is
  //     surfaced second so config overrides flow through.
  let toggle_key = app
    .bindings_for(Focus::List)
    .iter()
    .find(|b| b.action == Action::ToggleHelp)
    .map(|b| b.label.to_string())
    .unwrap_or_else(|| "?".to_string());
  let close_chip = format!("Esc/{toggle_key}:close");
  let block = Block::default()
    .title(Line::from(vec![
      Span::styled(
        " Help ",
        Style::default()
          .fg(palette.panel_title)
          .add_modifier(Modifier::BOLD),
      ),
      Span::styled(format!("· {close_chip} "), palette.muted_style()),
    ]))
    .borders(Borders::ALL)
    .border_style(palette.accent_style())
    .padding(Padding::new(2, 2, 1, 1));
  let inner = block.inner(rect);
  frame.render_widget(block, rect);

  let sections = build_sections(app);
  let columns = balance_into_columns(&sections, 3);

  let cols = Layout::default()
    .direction(Direction::Horizontal)
    .constraints([
      Constraint::Ratio(1, 3),
      Constraint::Ratio(1, 3),
      Constraint::Ratio(1, 3),
    ])
    .split(inner);

  for (idx, col_sections) in columns.iter().enumerate() {
    let lines = render_column(col_sections, palette);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), cols[idx]);
  }
}

/// One help-overlay section: a category title plus the resolved
/// rows (each a `(keys, description)` pair) drawn under it.
#[derive(Debug)]
struct Section {
  title: &'static str,
  rows: Vec<(String, String)>,
}

/// Walk `Category::ALL` in order; for each, collect every binding
/// whose `categories` contains that category. Bindings sharing an
/// action collapse to one row whose label is `","`-joined (so `c,y`
/// reads as a single curl row); their description comes from
/// `Action::description_for(category)` with `Binding::description`
/// as the fallback. Sections with no rows are dropped.
fn build_sections(app: &App) -> Vec<Section> {
  // Walk the flat keymap once via the List focus — its `bindings_for`
  // already strips per-focus duplicates, and the `categories` field
  // tells us where each row lands. For categories whose canonical
  // focus isn't List we re-resolve the labels under that focus so a
  // config override on (say) `Focus::HfDialog` flows through.
  let mut sections: Vec<Section> = Vec::with_capacity(Category::ALL.len());
  let flat: Vec<&Binding> = collect_flat(app);

  for &category in Category::ALL {
    // Group bindings landing in this category by action, preserving
    // first-seen order. Multiple bindings per action merge their
    // labels (e.g. `Up,k → up`).
    let mut action_order: Vec<Action> = Vec::new();
    let mut by_action: BTreeMap<usize, Vec<&Binding>> = BTreeMap::new();
    for b in &flat {
      if !b.categories.contains(&category) {
        continue;
      }
      let pos = action_order.iter().position(|a| *a == b.action);
      let idx = match pos {
        Some(i) => i,
        None => {
          action_order.push(b.action);
          action_order.len() - 1
        }
      };
      by_action.entry(idx).or_default().push(b);
    }

    let mut rows: Vec<(String, String)> = Vec::with_capacity(action_order.len());
    for (idx, action) in action_order.iter().enumerate() {
      let group = &by_action[&idx];
      let keys = group.iter().map(|b| b.label).collect::<Vec<_>>().join(",");
      let description = action
        .description_for(category)
        .map(|s| s.to_string())
        .unwrap_or_else(|| group[0].description().to_string());
      rows.push((keys, description));
    }
    if rows.is_empty() {
      continue;
    }
    sections.push(Section {
      title: category.label(),
      rows,
    });
  }
  sections
}

/// Snapshot the full keymap as a flat slice, deduplicating bindings
/// that appear under multiple focuses (the per-focus cache stores
/// the same binding once per focus).
fn collect_flat(app: &App) -> Vec<&Binding> {
  let mut seen: Vec<(
    crossterm::event::KeyCode,
    crossterm::event::KeyModifiers,
    Action,
  )> = Vec::new();
  let mut out: Vec<&Binding> = Vec::new();
  for focus in [
    Focus::List,
    Focus::Filter,
    Focus::RightPane,
    Focus::ChatInput,
    Focus::EmbedInput,
    Focus::RerankInput,
    Focus::ConfirmPopup,
    Focus::HfDialog,
  ] {
    for b in app.bindings_for(focus) {
      let key = (b.key, b.mods, b.action);
      if !seen.contains(&key) {
        seen.push(key);
        out.push(b);
      }
    }
  }
  out
}

/// Greedy column-packing: distribute `sections` into `n` columns so
/// the columns are roughly the same length (one line per row plus
/// title + blank trailer). Picks the shortest column at each step.
fn balance_into_columns(sections: &[Section], n: usize) -> Vec<Vec<&Section>> {
  let mut columns: Vec<Vec<&Section>> = vec![Vec::new(); n];
  let mut lengths: Vec<usize> = vec![0; n];
  for section in sections {
    let target = lengths
      .iter()
      .enumerate()
      .min_by_key(|(_, &l)| l)
      .map(|(i, _)| i)
      .unwrap_or(0);
    columns[target].push(section);
    // 1 title row + 1 row per binding + 1 trailing blank.
    lengths[target] += section.rows.len() + 2;
  }
  columns
}

fn render_column(sections: &[&Section], palette: &Palette) -> Vec<Line<'static>> {
  let mut out: Vec<Line<'static>> = Vec::new();
  for section in sections {
    out.push(Line::from(Span::styled(
      section.title.to_string(),
      Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD),
    )));
    for (keys, description) in &section.rows {
      out.push(render_binding_line(keys, description, palette));
    }
    out.push(Line::default());
  }
  out
}

fn render_binding_line(keys: &str, description: &str, palette: &Palette) -> Line<'static> {
  Line::from(vec![
    Span::styled("  ".to_string(), Style::default()),
    Span::styled(
      format!("{keys:<12}"),
      Style::default()
        .fg(palette.label)
        .add_modifier(Modifier::BOLD),
    ),
    Span::styled("  ".to_string(), Style::default()),
    Span::styled(description.to_string(), palette.text_style()),
  ])
}

/// Centre a `w × h` rect within `area`, clamping to the available
/// space so a narrow terminal still sees the overlay (just snug).
fn centred(area: Rect, w: u16, h: u16) -> Rect {
  let w = w.min(area.width.saturating_sub(2));
  let h = h.min(area.height.saturating_sub(2));
  let x = area.x + (area.width.saturating_sub(w)) / 2;
  let y = area.y + (area.height.saturating_sub(h)) / 2;
  Rect::new(x, y, w, h)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tui::app::AppOptions;
  use ratatui::backend::TestBackend;
  use ratatui::Terminal;

  fn render_to_string(width: u16, height: u16, app: &App) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test backend");
    terminal
      .draw(|frame| {
        let palette = crate::theme::palette_for(crate::theme::ThemeName::Macchiato);
        render(frame, frame.area(), app, palette);
      })
      .expect("draw");
    let buffer = terminal.backend().buffer();
    let mut s = String::new();
    for y in 0..buffer.area.height {
      for x in 0..buffer.area.width {
        s.push_str(buffer[(x, y)].symbol());
      }
      s.push('\n');
    }
    s
  }

  #[test]
  fn overlay_renders_global_section() {
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 36, &app);
    assert!(
      frame.contains("General"),
      "missing General section:\n{frame}"
    );
    assert!(frame.contains("quit"), "missing quit row:\n{frame}");
    assert!(frame.contains("help"), "missing help row:\n{frame}");
  }

  #[test]
  fn overlay_lists_motion_under_multiple_sections() {
    // `↑/↓ MoveUp/MoveDown` should surface under Models with the
    // default `"up"/"down"` text, and under Logs / Settings with
    // their category-specific overrides.
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 40, &app);
    assert!(
      frame.contains("Models list"),
      "Models section missing:\n{frame}"
    );
    assert!(frame.contains("Logs tab"), "Logs section missing:\n{frame}");
    assert!(
      frame.contains("Settings tab"),
      "Settings section missing:\n{frame}"
    );
    assert!(
      frame.contains("scroll up") && frame.contains("scroll down"),
      "Logs motion overrides missing:\n{frame}"
    );
    assert!(
      frame.contains("prev field") && frame.contains("next field"),
      "Settings motion overrides missing:\n{frame}"
    );
  }

  #[test]
  fn overlay_merges_aliased_chords() {
    // `c` and `y` both bind to YankCurl; the Models section should
    // surface them on one row as `c,y` rather than two separate rows.
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 40, &app);
    assert!(
      frame.contains("c,y"),
      "aliased yank chord missing:\n{frame}"
    );
  }

  #[test]
  fn overlay_shows_hf_pull_under_global() {
    // Shift+P (`OpenHfDialog`) is categorised as Global since it
    // works from any non-input focus.
    let app = App::new(AppOptions::default());
    let frame = render_to_string(140, 40, &app);
    assert!(frame.contains("pull"), "pull row missing:\n{frame}");
  }
}
