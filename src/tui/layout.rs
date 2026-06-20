//! Layout helpers shared by [`super::render`] and the modal
//! overlays. `centered_rect` frames percentage-sized modals (launch
//! picker, HF dialog); `centered_abs` frames fixed-cell overlays
//! (help, confirm) that reserve a margin so a narrow terminal still
//! sees the box.

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

/// Centre a `w × h` rect within `area`, clamping each axis to the
/// available space minus a reserved margin so a narrow terminal still
/// sees the overlay (just snug). `x_margin` / `y_margin` are the cells
/// each overlay reserves on that axis — the help overlay keeps a
/// 2-cell horizontal margin, the confirm overlay a 4-cell one.
pub fn centered_abs(area: Rect, w: u16, h: u16, x_margin: u16, y_margin: u16) -> Rect {
  let w = w.min(area.width.saturating_sub(x_margin));
  let h = h.min(area.height.saturating_sub(y_margin));
  let x = area.x + (area.width.saturating_sub(w)) / 2;
  let y = area.y + (area.height.saturating_sub(h)) / 2;
  Rect::new(x, y, w, h)
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

  #[test]
  fn centered_abs_centres_inside_area_with_margin() {
    let area = Rect::new(0, 0, 100, 40);
    let r = centered_abs(area, 60, 8, 4, 2);
    assert_eq!(r.width, 60);
    assert_eq!(r.height, 8);
    assert_eq!(r.x, 20);
    assert_eq!(r.y, 16);
  }

  #[test]
  fn centered_abs_clamps_to_available_space_minus_margin() {
    // Box wider/taller than the area: each axis clamps to the area
    // size minus that axis's reserved margin.
    let area = Rect::new(0, 0, 30, 10);
    let r = centered_abs(area, 60, 20, 4, 2);
    assert_eq!(r.width, 30 - 4);
    assert_eq!(r.height, 10 - 2);
  }
}
