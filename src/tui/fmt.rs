//! Shared display helpers for the TUI panes.
//!
//! Centralizing these formatters avoids the silent drift that crept in
//! when three panes each defined their own `format_bytes` with subtly
//! different thresholds.

// `panel_title` moved to `Palette::title_style()` / `Palette::panel_block`
// during the Tier-B sweep — see `src/theme/palette.rs`.

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use crate::theme::Palette;

/// Single caret span (`▏` painted in `palette.accent` + REVERSED).
/// Used by every single-line text input so the cursor reads
/// identically across the TUI (audit §F2.1 #4 — replaces three
/// open-coded `Span::styled("▏", …)` sites in `advanced_panel`,
/// `list_pane`'s filter chip, and `tabs/input_pane`).
pub(crate) fn caret(palette: &Palette) -> Span<'static> {
  Span::styled(
    "▏",
    Style::default()
      .fg(palette.accent)
      .add_modifier(Modifier::REVERSED),
  )
}

/// Format a token count for the Ctx column / launch picker:
/// `131072` → `128k`, `262144` → `256k`, `2_000_000` → `2.0M`.
/// Sub-1024 values render as raw integers (e.g., `512`).
pub(crate) fn format_tokens(n: u64) -> String {
  const K: u64 = 1024;
  const M: u64 = K * 1024;
  if n >= M {
    let m = n as f64 / M as f64;
    if m >= 10.0 {
      format!("{m:.0}M")
    } else {
      format!("{m:.1}M")
    }
  } else if n >= K {
    let k = n as f64 / K as f64;
    if k >= 10.0 {
      format!("{k:.0}k")
    } else {
      format!("{k:.1}k")
    }
  } else {
    n.to_string()
  }
}

/// Format a `used/total` byte pair using one shared unit suffix taken
/// from `total`, so `MEM` / `VRAM` rows render as `2.5/4.0G` rather
/// than `2.5G/4.0G`. Each value follows the same 1-decimal-under-10
/// rule as [`format_bytes`].
pub(crate) fn format_bytes_pair(used: u64, total: u64) -> String {
  const KIB: u64 = 1024;
  const MIB: u64 = KIB * 1024;
  const GIB: u64 = MIB * 1024;
  let (div, suffix): (u64, &str) = if total >= GIB {
    (GIB, "G")
  } else if total >= MIB {
    (MIB, "M")
  } else if total >= KIB {
    (KIB, "K")
  } else {
    (1, "B")
  };
  let one = |b: u64| -> String {
    let v = b as f64 / div as f64;
    if v >= 10.0 {
      format!("{v:.0}")
    } else {
      format!("{v:.1}")
    }
  };
  format!("{}/{}{suffix}", one(used), one(total))
}

/// Format a byte count for compact display in panel headers and bars.
/// Rounds to a single decimal place between 1G and 10G (so `4.2G` is
/// distinguishable from `5.1G`), and drops the decimal at 10G+ to keep
/// the label inside ~4 characters.
pub(crate) fn format_bytes(bytes: u64) -> String {
  const KIB: f64 = 1024.0;
  const MIB: f64 = KIB * 1024.0;
  const GIB: f64 = MIB * 1024.0;
  let b = bytes as f64;
  if b >= GIB {
    let g = b / GIB;
    if g >= 10.0 {
      format!("{g:.0}G")
    } else {
      format!("{g:.1}G")
    }
  } else if b >= MIB {
    format!("{:.0}M", b / MIB)
  } else if b >= KIB {
    format!("{:.0}K", b / KIB)
  } else {
    format!("{bytes}B")
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn format_tokens_basic_ranges() {
    assert_eq!(format_tokens(0), "0");
    assert_eq!(format_tokens(512), "512");
    assert_eq!(format_tokens(1024), "1.0k");
    assert_eq!(format_tokens(2048), "2.0k");
    assert_eq!(format_tokens(8192), "8.0k");
    assert_eq!(format_tokens(32_768), "32k");
    assert_eq!(format_tokens(131_072), "128k");
    assert_eq!(format_tokens(262_144), "256k");
    assert_eq!(format_tokens(1_048_576), "1.0M");
    assert_eq!(format_tokens(2_097_152), "2.0M");
    assert_eq!(format_tokens(10_485_760), "10M");
  }

  #[test]
  fn under_kib_renders_raw_bytes() {
    assert_eq!(format_bytes(0), "0B");
    assert_eq!(format_bytes(512), "512B");
    assert_eq!(format_bytes(1023), "1023B");
  }

  #[test]
  fn kib_and_mib_drop_decimals() {
    assert_eq!(format_bytes(1024), "1K");
    assert_eq!(format_bytes(1024 * 1024), "1M");
  }

  #[test]
  fn gib_below_ten_keeps_one_decimal() {
    assert_eq!(format_bytes(4_500_000_000), "4.2G");
    assert_eq!(format_bytes(9_000_000_000), "8.4G");
  }

  #[test]
  fn gib_at_or_above_ten_drops_decimal() {
    assert_eq!(format_bytes(11_000_000_000), "10G");
    assert_eq!(format_bytes(24_000_000_000), "22G");
    assert_eq!(format_bytes(100_000_000_000), "93G");
  }

  #[test]
  fn format_bytes_pair_shares_unit_suffix() {
    // Regression: RAM / VRAM rows used to render `66G/121G` and
    // `2.5G/4.0G` — unit repeated on both sides. Both values now share
    // one trailing suffix taken from `total`.
    const GIB: u64 = 1024 * 1024 * 1024;
    assert_eq!(format_bytes_pair(66 * GIB, 121 * GIB), "66/121G");
    assert_eq!(
      format_bytes_pair(2_642_341_888, 4 * GIB),
      "2.5/4.0G",
      "VRAM at <10G should keep one decimal on both sides"
    );
  }

  #[test]
  fn format_bytes_pair_scales_used_to_total_unit() {
    // `500M/2.0G` would mix units; pair scales `used` down into the
    // larger unit (`0.2/2.0G`) so the suffix stays consistent.
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    assert_eq!(format_bytes_pair(500 * MIB, 2 * GIB), "0.5/2.0G");
  }

  #[test]
  fn format_bytes_pair_at_or_above_ten_drops_decimal() {
    const GIB: u64 = 1024 * 1024 * 1024;
    assert_eq!(format_bytes_pair(14 * GIB, 32 * GIB), "14/32G");
  }

  #[test]
  fn format_bytes_pair_falls_back_to_bytes_for_tiny_totals() {
    assert_eq!(format_bytes_pair(0, 512), "0.0/512B");
  }
}
