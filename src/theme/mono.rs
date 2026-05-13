use ratatui::style::Color;

use super::palette::{Palette, ThemeName};

// Monochrome — relies on glyph cues plus bold/reverse modifiers (applied at
// render sites) since colour cannot carry meaning here. Status colours fall
// back to terminal defaults so the layer above can use Modifier::BOLD /
// Modifier::REVERSED for emphasis.
pub(crate) const PALETTE: Palette = Palette {
  name: ThemeName::Mono,
  is_dark: true,
  bg: Color::Reset,
  fg: Color::White,
  accent: Color::White,
  success: Color::White,
  warning: Color::Gray,
  error: Color::White,
  muted: Color::DarkGray,
  selection: Color::DarkGray,
  status_loading: Color::Gray,
  status_ready: Color::White,
  status_error: Color::White,
  status_stopped: Color::DarkGray,
  status_external: Color::Gray,
};
