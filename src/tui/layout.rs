//! Layout helpers shared by [`super::render`] and the modal
//! overlays.
//!
//! v1 keeps these to a single utility — `centered_rect` — used by
//! the launch picker and advanced panel for their modal framing.
//! Inline rather than re-exported so the call site reads as
//! `layout::centered_rect(60, 30, area)`.

use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Compute a centred rectangle with the supplied width/height
/// percentages.
pub fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
  let v = Layout::default()
    .direction(Direction::Vertical)
    .constraints([
      Constraint::Percentage((100 - pct_y) / 2),
      Constraint::Percentage(pct_y),
      Constraint::Percentage((100 - pct_y) / 2),
    ])
    .split(area);
  Layout::default()
    .direction(Direction::Horizontal)
    .constraints([
      Constraint::Percentage((100 - pct_x) / 2),
      Constraint::Percentage(pct_x),
      Constraint::Percentage((100 - pct_x) / 2),
    ])
    .split(v[1])[1]
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn centered_rect_at_full_size_equals_input_area() {
    let area = Rect::new(0, 0, 80, 24);
    let r = centered_rect(100, 100, area);
    assert_eq!(r, area);
  }

  #[test]
  fn centered_rect_50pct_lands_inside_input_area() {
    let area = Rect::new(0, 0, 100, 100);
    let r = centered_rect(50, 50, area);
    assert!(r.x >= 25 && r.y >= 25);
    assert_eq!(r.width, 50);
    assert_eq!(r.height, 50);
  }
}
