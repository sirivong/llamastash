//! Per-state status glyph + colour pair used in the model list and
//! the right-pane header.
//!
//! Plan-mandated dual encoding: every state carries both a colour
//! (from the active palette) AND a unique glyph, so colour-blind
//! users (or terminals stripped of colour by the user's
//! configuration) still distinguish states.

use ratatui::style::Color;

use crate::theme::Palette;

/// Generic surface state used by the TUI list pane. Distinct from
/// `daemon::supervisor::ManagedState` because the TUI also wants to
/// render rows that have *no* launch (`NotLaunched`) and rows for
/// external read-only processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceState {
  NotLaunched,
  Launching,
  Loading,
  Ready,
  Error,
  Stopped,
  External,
}

/// Glyph and palette colour for a state. Renderers call
/// `glyph_for(state)` and `colour_for(state, palette)` directly to
/// keep the dual encoding in lock-step.
pub fn glyph_for(state: SurfaceState) -> char {
  match state {
    SurfaceState::NotLaunched => ' ',
    SurfaceState::Launching => '◌',
    SurfaceState::Loading => '◐',
    SurfaceState::Ready => '●',
    SurfaceState::Error => '▲',
    SurfaceState::Stopped => '○',
    SurfaceState::External => '⇪',
  }
}

pub fn colour_for(state: SurfaceState, palette: &Palette) -> Color {
  match state {
    SurfaceState::NotLaunched => palette.muted,
    SurfaceState::Launching | SurfaceState::Loading => palette.status_loading,
    SurfaceState::Ready => palette.status_ready,
    SurfaceState::Error => palette.status_error,
    SurfaceState::Stopped => palette.status_stopped,
    SurfaceState::External => palette.status_external,
  }
}

/// Short label for a state — used by the right-pane header and the
/// CLI's human-readable status output.
pub fn label_for(state: SurfaceState) -> &'static str {
  match state {
    SurfaceState::NotLaunched => "—",
    SurfaceState::Launching => "Launching",
    SurfaceState::Loading => "Loading",
    SurfaceState::Ready => "Ready",
    SurfaceState::Error => "Error",
    SurfaceState::Stopped => "Stopped",
    SurfaceState::External => "External",
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use crate::theme::{palette_for, ThemeName};

  #[test]
  fn every_state_has_a_distinct_glyph() {
    use std::collections::HashSet;
    let states = [
      SurfaceState::Launching,
      SurfaceState::Loading,
      SurfaceState::Ready,
      SurfaceState::Error,
      SurfaceState::Stopped,
      SurfaceState::External,
    ];
    let glyphs: HashSet<char> = states.iter().copied().map(glyph_for).collect();
    assert_eq!(
      glyphs.len(),
      states.len(),
      "each surface state must have a unique glyph for colour-blind users"
    );
  }

  #[test]
  fn colour_for_picks_palette_status_slot() {
    let p = palette_for(ThemeName::Macchiato);
    assert_eq!(colour_for(SurfaceState::Ready, p), p.status_ready);
    assert_eq!(colour_for(SurfaceState::Error, p), p.status_error);
  }

  #[test]
  fn label_is_stable_for_each_variant() {
    assert_eq!(label_for(SurfaceState::Ready), "Ready");
    assert_eq!(label_for(SurfaceState::Launching), "Launching");
    assert_eq!(label_for(SurfaceState::External), "External");
  }
}
