//! Model list rows for the left pane.
//!
//! Four row variants live here:
//!  1. [`ListRow::TableHeader`] — the column-label row pinned to the
//!     very top of the list. Always first, never selectable, kept in
//!     lock-step with the per-model row layout so the columns align.
//!  2. [`ListRow::Header`] — folder-group / `★ Favorites` section
//!     header that introduces the rows beneath it.
//!  3. [`ListRow::Model`] — one rendered row per discovered GGUF.
//!  4. [`ListRow::Divider`] — thin horizontal rule injected between
//!     sections that share content (e.g. after `★ Favorites` when
//!     folder groups follow, since favorited models also appear in
//!     their original folder).
//!
//! Per-state colour: rows pick their foreground from the surface
//! state — `Ready` is rendered with `palette.success`, `Error` with
//! `palette.error`, everything else with `palette.fg`. The selected
//! row uses `Modifier::REVERSED` so fg/bg flip at the terminal layer,
//! which means a running row stays visibly "green" even when it's
//! the highlighted row (it just inverts).

use std::collections::BTreeMap;
use std::path::PathBuf;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};
use ratatui::Frame;

use crate::discovery::DiscoveredModel;
use crate::theme::Palette;
use crate::tui::fmt::{format_bytes, format_tokens};
use crate::tui::status_icons::{colour_for, glyph_for, SurfaceState};

/// A row as it appears in the rendered list — table header, group
/// header, or model row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListRow {
  /// Always-on column-label row pinned to the top of the list.
  /// Renders the same fixed-width column grid as every model row so
  /// the column labels align with their values.
  TableHeader,
  /// Section header (Favorites, or one parent directory).
  Header { label: String },
  /// Visual separator (horizontal rule) between sections. Not
  /// selectable. Currently injected only after the `★ Favorites`
  /// block when folder groups follow, so the user can tell the
  /// favorited rows above apart from the same favorited rows
  /// reappearing in their parent folder below.
  Divider,
  /// One model row. `state` is the surface-level lifecycle for any
  /// active launch of this model, or `NotLaunched` when no
  /// supervisor has touched it.
  Model {
    /// Canonical path — `Model` rows match against the daemon's
    /// `start_model` API which expects this same path.
    path: PathBuf,
    /// User-facing display name (file stem).
    name: String,
    /// Architecture badge (e.g. `llama`, `qwen3`); empty when
    /// metadata is unavailable.
    arch: String,
    /// Quantisation badge (e.g. `Q4_K`, `Q8_0`).
    quant: String,
    /// Native context length in tokens, when known.
    native_ctx: Option<u64>,
    /// Weights footprint in bytes (sum of tensor storage), when
    /// known.
    weights_bytes: Option<u64>,
    /// Mode hint surfaced at discovery time.
    mode_hint: String,
    /// Whether this row is favorited (drives the `★` glyph).
    favorite: bool,
    state: SurfaceState,
    /// Port the row's launch (if any) is listening on. `None` for
    /// rows that aren't currently running — the column renders `—`
    /// in that case so the slot stays aligned across the table.
    port: Option<u16>,
    /// Launch identity the row is bound to. `Some(id)` only for
    /// rows in the `▶ Running` group — that's how the right pane
    /// disambiguates between duplicate launches of the same model.
    /// `None` for rows in Favorites / folders / Recent (those
    /// resolve their managed launch by path, if any).
    launch_id: Option<String>,
  },
}

impl ListRow {
  pub fn is_selectable(&self) -> bool {
    matches!(self, ListRow::Model { .. })
  }

  pub fn path(&self) -> Option<&std::path::Path> {
    match self {
      ListRow::Model { path, .. } => Some(path),
      ListRow::Header { .. } | ListRow::TableHeader | ListRow::Divider => None,
    }
  }
}

/// Inputs to [`build_rows`]. `model_states` is a snapshot of every
/// `(model_path → surface_state)` pair the daemon reports as
/// supervised; rows for paths absent from the map land as
/// `NotLaunched`. `model_ports` carries the same set of paths to
/// their bound port so the row can render `:12345` instead of `—`
/// in the Port column. `running` carries one entry per active
/// launch (path may repeat for duplicate launches of the same
/// model) — these become the `▶ Running` section at the top of
/// the list. `recent_paths` is the persisted top-N recently-
/// launched paths the `↺ Recent` section surfaces; entries whose
/// path is currently running are skipped so the same model isn't
/// shown in both groups.
pub struct RowInputs<'a> {
  pub models: &'a [DiscoveredModel],
  pub favorites: &'a [PathBuf],
  pub model_states: &'a BTreeMap<PathBuf, SurfaceState>,
  pub model_ports: &'a BTreeMap<PathBuf, u16>,
  pub running: &'a [RunningLaunchRow],
  pub recent_paths: &'a [PathBuf],
}

/// One active managed launch the `▶ Running` group should render.
/// Mirrors the subset of `app::ManagedRow` the list pane cares about
/// without coupling `list_pane` to the heavier ManagedRow type.
#[derive(Debug, Clone)]
pub struct RunningLaunchRow {
  pub launch_id: String,
  pub path: PathBuf,
  pub port: u16,
  pub state: SurfaceState,
}

/// Group `models` into:
///  - [`ListRow::TableHeader`] (always row 0, even when there's
///    nothing else; the renderer makes the column labels themselves
///    a visible signal that the pane is loaded).
///  - `★ Favorites` (only when at least one favorited model is in
///    the list).
///  - One section per parent directory, alphabetical by parent.
///    Rows within a group sort by display name.
pub fn build_rows(inputs: RowInputs<'_>) -> Vec<ListRow> {
  // No discovered models → no rows at all. The renderer detects an
  // empty Vec and switches to `render_empty_state`, which paints the
  // "drop a .gguf …" hint. Returning `[TableHeader]` here would
  // shadow that hint with a lone column-label row.
  if inputs.models.is_empty() {
    return Vec::new();
  }
  let mut rows: Vec<ListRow> = Vec::with_capacity(inputs.models.len() + inputs.running.len() + 6);
  rows.push(ListRow::TableHeader);

  let favorite_set: std::collections::BTreeSet<&PathBuf> = inputs.favorites.iter().collect();

  // Build a `path → DiscoveredModel` lookup so the Running and
  // Recent sections can synthesise rows even when their paths
  // aren't in the favorites/folder groupings below.
  let by_path: BTreeMap<&PathBuf, &DiscoveredModel> =
    inputs.models.iter().map(|m| (&m.path, m)).collect();

  // ▶ Running — one row per active managed launch. Order comes
  // from the caller (App preserves latest-first across status
  // ticks) so a re-launch of an existing model still bubbles to
  // the top of the group.
  if !inputs.running.is_empty() {
    rows.push(ListRow::Header {
      label: "▶ Running".into(),
    });
    for launch in inputs.running {
      // If the path doesn't appear in the catalog (e.g. an
      // adopted external launch the discovery sweep hasn't yet
      // surfaced), synthesise a thin row so the launch still
      // shows up. Most launches resolve to the catalog and pull
      // metadata cleanly.
      let row = match by_path.get(&launch.path) {
        Some(m) => running_row(m, launch),
        None => running_row_stub(launch),
      };
      rows.push(row);
    }
  }

  // ↺ Recent — top of the persisted "last launched" history that
  // isn't already shown in Running. Filtered against `by_path` so
  // entries pointing at vanished GGUFs don't surface as ghost rows.
  let running_paths: std::collections::BTreeSet<&PathBuf> =
    inputs.running.iter().map(|r| &r.path).collect();
  let recent_visible: Vec<&DiscoveredModel> = inputs
    .recent_paths
    .iter()
    .filter(|p| !running_paths.contains(*p))
    .filter_map(|p| by_path.get(p).copied())
    .collect();
  if !recent_visible.is_empty() {
    rows.push(ListRow::Header {
      label: "↺ Recent".into(),
    });
    for m in recent_visible {
      let fav = favorite_set.contains(&m.path);
      rows.push(model_row(
        m,
        fav,
        surface_state_for(m, inputs.model_states),
        inputs.model_ports.get(&m.path).copied(),
        None,
      ));
    }
  }

  // Pre-compute the folder grouping so we know whether to inject a
  // divider after the Favorites section. Running paths drop out
  // because the user already sees a live row up top — the catalog
  // representation would be noise. Favorited paths are *kept*: the
  // user expects to find them in their original folder, the
  // `★ Favorites` section is an extra shortcut, not a relocation.
  let mut grouped: BTreeMap<&PathBuf, Vec<&DiscoveredModel>> = BTreeMap::new();
  for m in inputs.models {
    if !running_paths.contains(&m.path) {
      grouped.entry(&m.parent).or_default().push(m);
    }
  }
  let has_folder_groups = !grouped.is_empty();

  // Favorites section. Paths currently in the Running group drop
  // out so a model never appears as both a live row and a starred
  // shortcut — the user already sees the live row up top.
  let favorites: Vec<&DiscoveredModel> = inputs
    .models
    .iter()
    .filter(|m| favorite_set.contains(&m.path))
    .filter(|m| !running_paths.contains(&m.path))
    .collect();
  if !favorites.is_empty() {
    rows.push(ListRow::Header {
      label: "★ Favorites".into(),
    });
    let mut sorted = favorites.clone();
    sorted.sort_by_key(|a| display_name(a));
    for m in sorted {
      rows.push(model_row(
        m,
        true,
        surface_state_for(m, inputs.model_states),
        inputs.model_ports.get(&m.path).copied(),
        None,
      ));
    }
    // Visual separator between favorites and folder groups —
    // favorited rows reappear in their parent folder below, so a
    // thin rule helps the eye tell the two surfaces apart. Skipped
    // when there's nothing after to separate from.
    if has_folder_groups {
      rows.push(ListRow::Divider);
    }
  }

  // Folder groups — one section per parent directory, alphabetical
  // by parent. Favorited rows reappear here with their `★` glyph
  // preserved.
  for (parent, mut entries) in grouped {
    rows.push(ListRow::Header {
      label: crate::util::paths::friendly_group_label(parent),
    });
    entries.sort_by_key(|a| display_name(a));
    for m in entries {
      let fav = favorite_set.contains(&m.path);
      rows.push(model_row(
        m,
        fav,
        surface_state_for(m, inputs.model_states),
        inputs.model_ports.get(&m.path).copied(),
        None,
      ));
    }
  }

  rows
}

fn running_row(m: &DiscoveredModel, launch: &RunningLaunchRow) -> ListRow {
  let mut row = model_row(
    m,
    false,
    launch.state,
    Some(launch.port),
    Some(launch.launch_id.clone()),
  );
  // The favorite glyph drops on Running rows so two launches of the
  // same favorited model don't both wear a star — the original star
  // still shows on the row in its folder group below (the dedicated
  // `★ Favorites` section filters running paths out, so the folder
  // group is the canonical home for a running favorite). Achieved by
  // passing `favorite=false` above.
  if let ListRow::Model { ref mut path, .. } = row {
    // `model_row` cloned the catalog path; preserve as-is.
    let _ = path;
  }
  row
}

fn running_row_stub(launch: &RunningLaunchRow) -> ListRow {
  ListRow::Model {
    path: launch.path.clone(),
    name: launch
      .path
      .file_stem()
      .and_then(|s| s.to_str())
      .unwrap_or("(unknown)")
      .to_string(),
    arch: String::new(),
    quant: String::new(),
    native_ctx: None,
    weights_bytes: None,
    mode_hint: "unknown".into(),
    favorite: false,
    state: launch.state,
    port: Some(launch.port),
    launch_id: Some(launch.launch_id.clone()),
  }
}

fn surface_state_for(
  m: &DiscoveredModel,
  states: &BTreeMap<PathBuf, SurfaceState>,
) -> SurfaceState {
  states
    .get(&m.path)
    .copied()
    .unwrap_or(SurfaceState::NotLaunched)
}

fn display_name(m: &DiscoveredModel) -> String {
  // `display_label` is populated by sources where the file basename
  // is hostile (Ollama's content-addressed `sha256-<hex>` blobs). For
  // every other source (HF cache, LM Studio, user paths) we render
  // the full file_stem so the user always sees the unambiguous
  // weight identifier — quant, finetune, variant.
  if let Some(label) = &m.display_label {
    return label.clone();
  }
  m.path
    .file_stem()
    .and_then(|s| s.to_str())
    .map(|s| s.to_string())
    .unwrap_or_else(|| m.path.display().to_string())
}

fn model_row(
  m: &DiscoveredModel,
  favorite: bool,
  state: SurfaceState,
  port: Option<u16>,
  launch_id: Option<String>,
) -> ListRow {
  let (arch, quant, native_ctx, mode_hint, weights_bytes) = match &m.metadata {
    Some(md) => (
      md.arch.clone().unwrap_or_default(),
      md.quant.label().to_string(),
      md.native_ctx,
      mode_hint_label(md.mode_hint),
      md.weights_bytes,
    ),
    None => (String::new(), String::new(), None, "unknown".into(), None),
  };
  ListRow::Model {
    path: m.path.clone(),
    name: display_name(m),
    arch,
    quant,
    native_ctx,
    weights_bytes,
    mode_hint,
    favorite,
    state,
    port,
    launch_id,
  }
}

fn mode_hint_label(hint: crate::gguf::metadata::ModeHint) -> String {
  use crate::gguf::metadata::ModeHint;
  match hint {
    ModeHint::Chat => "chat".into(),
    ModeHint::Embedding => "embedding".into(),
    ModeHint::Rerank => "rerank".into(),
    ModeHint::Unknown => "unknown".into(),
  }
}

// ── Table column geometry ────────────────────────────────────────
//
// The pane is wrapped in a bordered Block, so ratatui reserves 1
// column on each side. Inside the borders the rendered widget then
// reserves a 2-cell highlight gutter (for the `> ` selection
// arrow). What remains is the budget the table grid lives in.
//
// All five right-side columns are fixed width so headers align with
// values. The `Name` column flexes to fill whatever's left and is
// ellipsised when the discovered name overflows it.

// No external highlight symbol/gutter — rows render flush to the
// inner edge. The selection cursor (`=>`) is painted INSIDE the
// favorite-marker slot instead so we don't burn a column on a
// near-always-empty gutter.
const HIGHLIGHT_SYMBOL: &str = "";
const HIGHLIGHT_GUTTER: usize = 0;
/// Single combined marker column: hosts the selection cursor, the
/// launch-state glyph, or the favorite star depending on priority
/// (see [`marker_span`]). Shape is `" {char} "` — leading space for
/// border breathing room, glyph, trailing separator. Replaces the
/// pre-split `STATUS_W + FAV_W = 6` chrome with a flat 3 cells.
const MARKER_W: usize = 3;
const COL_SEP_W: usize = 1; // space before each data column
/// Minimum number of columns the Name column always reserves.
const MIN_NAME_W: usize = 8;

/// Identifies which model field a data column renders. Lets the
/// [`Column`] table stay `const` while the value extraction lives
/// at the call site (the row's per-field data isn't in scope at
/// table-definition time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnId {
  Arch,
  Quant,
  Ctx,
  Size,
  Mode,
  Port,
}

/// One data column in the right-side strip. `rank` decides which
/// columns survive under width pressure (lower = stickier — see
/// [`layout_columns`]); the declaration order in [`COLUMNS`] is the
/// canonical left-to-right display order, so visible columns keep
/// their familiar positions as the terminal resizes — only the
/// less-important ones drop out.
#[derive(Debug, Clone, Copy)]
struct Column {
  id: ColumnId,
  label: &'static str,
  width: usize,
  rank: u8,
}

/// Right-side data columns, ordered left-to-right as they appear in
/// the rendered list. Ranks are explicit (lower = stickier):
///
/// - `Size` (10): top decision driver — "will it fit in VRAM".
/// - `Quant` (20): quality/fit signal, second most asked.
/// - `Ctx`   (30): use-case fit (context length).
/// - `Arch`  (40): mostly inferable from the name.
/// - `Mode`  (50): almost always `Chat`; low entropy.
/// - `Port`  (60): only meaningful for the small subset of running
///   rows, and the marker glyph already encodes "this is running".
///
/// Source order is the display order. Picker reorders by rank only
/// for the visibility decision.
const COLUMNS: &[Column] = &[
  Column {
    id: ColumnId::Arch,
    label: "Arch",
    width: 8,
    rank: 40,
  },
  Column {
    id: ColumnId::Quant,
    label: "Quant",
    width: 7,
    rank: 20,
  },
  Column {
    id: ColumnId::Ctx,
    label: "Ctx",
    width: 7,
    rank: 30,
  },
  Column {
    id: ColumnId::Size,
    label: "Size",
    width: 6,
    rank: 10,
  },
  Column {
    id: ColumnId::Mode,
    label: "Mode",
    width: 10,
    rank: 50,
  },
  // `:port` for a u16 maxes at 6 cells (`:65535`); the column
  // stays flush at 6 so the header label "Port" lines up.
  Column {
    id: ColumnId::Port,
    label: "Port",
    width: 6,
    rank: 60,
  },
];

/// Layout decision for one render pass: which data columns survive
/// the width budget, and how many cells are left for the flexible
/// Name column.
struct ColumnLayout {
  /// Columns to render, in left-to-right display order (source
  /// order from [`COLUMNS`]). Empty when the pane is too narrow
  /// for any data column.
  visible: Vec<&'static Column>,
  /// Cells the Name column gets after the marker and visible data
  /// columns are subtracted. Floors at [`MIN_NAME_W`].
  name_w: usize,
}

/// Greedy-fit data columns into the budget left after marker +
/// `MIN_NAME_W` are reserved. Lower-rank columns win first. Any
/// budget the picker doesn't spend rolls back into the Name column
/// so a wide pane gives Name room to breathe.
fn layout_columns(content_w: usize) -> ColumnLayout {
  let budget = content_w.saturating_sub(MARKER_W + MIN_NAME_W);
  let mut by_rank: Vec<(usize, &'static Column)> = COLUMNS.iter().enumerate().collect();
  by_rank.sort_by_key(|(_, c)| c.rank);

  let mut taken: Vec<(usize, &'static Column)> = Vec::with_capacity(COLUMNS.len());
  let mut spent = 0usize;
  for (idx, c) in by_rank {
    let cost = c.width + COL_SEP_W;
    if spent + cost <= budget {
      spent += cost;
      taken.push((idx, c));
    }
  }
  // Restore declaration order so columns disappear from less-
  // important slots while the survivors keep their familiar
  // positions on screen.
  taken.sort_by_key(|(idx, _)| *idx);
  let visible: Vec<&'static Column> = taken.into_iter().map(|(_, c)| c).collect();

  let name_w = content_w
    .saturating_sub(MARKER_W + spent)
    .max(MIN_NAME_W);
  ColumnLayout { visible, name_w }
}

/// Filter-input state for the Models block title. When the filter
/// is active the `/:filter` chip is replaced by an inline input
/// containing the buffered query; `focused=true` adds the block
/// cursor (rendered via `Modifier::REVERSED`).
#[derive(Debug, Clone, Copy)]
pub enum FilterTitle<'a> {
  Inactive,
  Active { buffer: &'a str, focused: bool },
}

/// Inputs to `build_block_title`. Bundled so the title call site
/// in `render` and `render_empty_state` doesn't drift; adding a new
/// piece of context (e.g. a "stale catalog" badge) only touches
/// one place.
///
/// `hints` is the resolved chip strip — the caller (`render.rs`)
/// builds it via `App::hint` so config-driven key overrides flow
/// through to the title automatically. Hints are dropped from the
/// tail under budget pressure; order them most-important-first.
#[derive(Debug, Clone)]
pub struct TitleInputs<'a> {
  pub total: usize,
  pub area_width: usize,
  pub filter: FilterTitle<'a>,
  pub hints: Vec<String>,
}

/// Border colour for the Models pane based on focus. Delegates to
/// `Palette::focus_border` so every focus indicator across the TUI
/// reads with the same theme-aware tone (`highlight` when set,
/// `accent` fallback for Mono). Re-used by the empty-state path in
/// `render.rs` so both surfaces share one focus rule.
pub fn border_color(palette: &Palette, focused: bool) -> Color {
  palette.focus_border(focused)
}

/// Compose the bottom-edge status legend that explains every
/// surface-state glyph used in the row list. Rendered into the
/// block's bottom title so it's always visible without spending a
/// content row. Each glyph carries its semantic palette colour;
/// the labels are muted so the strip reads as a hint, not data.
fn build_status_legend(palette: &Palette) -> Line<'static> {
  use crate::tui::status_icons::{colour_for, glyph_for, SurfaceState};
  let entries: &[(SurfaceState, &str)] = &[
    (SurfaceState::Launching, "launching"),
    (SurfaceState::Loading, "loading"),
    (SurfaceState::Ready, "ready"),
    (SurfaceState::Error, "error"),
    (SurfaceState::Stopped, "stopped"),
    (SurfaceState::External, "external"),
  ];
  let mut spans: Vec<Span<'static>> = Vec::with_capacity(entries.len() * 4 + 2);
  spans.push(Span::raw(" "));
  for (i, (state, label)) in entries.iter().enumerate() {
    if i > 0 {
      spans.push(Span::styled(" · ", palette.muted_style()));
    }
    spans.push(Span::styled(
      glyph_for(*state).to_string(),
      Style::default().fg(colour_for(*state, palette)),
    ));
    spans.push(Span::raw(" "));
    spans.push(Span::styled((*label).to_string(), palette.muted_style()));
  }
  spans.push(Span::raw(" "));
  Line::from(spans)
}

/// Inputs to [`render`] — bundled into a struct so the call site
/// stays readable and the function stays under clippy's
/// `too_many_arguments` threshold. `filter_chip_label` is the
/// resolved `/:filter` label (live from the keymap) used when the
/// filter is inactive; `focused` paints the focus border colour.
pub struct RenderInputs<'a> {
  pub rows: &'a [ListRow],
  pub selected: usize,
  pub title: TitleInputs<'a>,
  pub filter_chip_label: &'a str,
  pub focused: bool,
}

/// Render `rows` into the supplied area using the active palette.
pub fn render(frame: &mut Frame<'_>, area: Rect, palette: &Palette, input: RenderInputs<'_>) {
  // Width inside the borders is `area.width - 2`. Subtract the
  // highlight gutter ratatui reserves for the selection marker
  // (`HIGHLIGHT_GUTTER` cells on every row, even unselected ones,
  // so columns stay column-aligned).
  let inner_w = area.width.saturating_sub(2) as usize;
  let content_w = inner_w.saturating_sub(HIGHLIGHT_GUTTER);
  let layout = layout_columns(content_w);

  let rows = input.rows;
  let safe_selected = if rows.is_empty() {
    None
  } else {
    Some(input.selected.min(rows.len().saturating_sub(1)))
  };
  let items: Vec<ListItem<'_>> = rows
    .iter()
    .enumerate()
    .map(|(i, r)| {
      let is_selected = Some(i) == safe_selected;
      render_row(r, palette, &layout, content_w, is_selected)
    })
    .collect();
  let title_line = build_block_title(input.title, input.filter_chip_label, palette, input.focused);
  let legend = build_status_legend(palette);
  let border_color = border_color(palette, input.focused);
  let list = List::new(items)
    .block(
      Block::default()
        .title(title_line)
        .title_bottom(legend)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color)),
    )
    // KDash-style row highlight: paint the focused row with a
    // saturated `highlight` background (gold/amber per theme) and
    // flip the foreground to the panel `bg` so the row reads with
    // strong contrast against the muted body. Themes that opt out
    // (mono: `highlight = Color::Reset`) fall back to plain
    // `Modifier::REVERSED` so the inverted-block idiom still works
    // without colour. Each `ListItem` leaves its cell fg unset so
    // the override here applies uniformly across the row.
    .highlight_style(highlight_style(palette))
    .highlight_symbol(HIGHLIGHT_SYMBOL);
  let mut state = ListState::default();
  state.select(safe_selected);
  frame.render_stateful_widget(list, area, &mut state);
}

/// KDash-style selected-row paint: a pure `REVERSED` inversion of
/// the row's own foreground. This is the "inverse of the row colour"
/// effect — a Ready (green) row paints a green bar with the panel
/// `bg` showing through the text; an Error (red) row paints red;
/// normal rows paint with `palette.fg`. Works uniformly across every
/// theme (including mono) because the swap is purely a modifier and
/// doesn't depend on a `highlight` colour slot.
fn highlight_style(_palette: &Palette) -> Style {
  Style::default()
    .add_modifier(Modifier::REVERSED)
    .add_modifier(Modifier::BOLD)
}

/// Build the Models block title as a styled `Line`. Order
/// (left-to-right):
///   ` Models [N] ` · `/:filter` chip (or inline buffer when active)
///   · hint chips from `input.hints`.
///
/// `input.hints` is pre-resolved against the live keymap by the
/// caller, so config overrides flow through automatically. The
/// filter slot itself comes from the `Focus::List` `OpenFilter`
/// binding so its key label tracks user rebinds too. Hints drop
/// from the tail under budget pressure; the count, the filter slot,
/// and `hints[0]` are never dropped.
pub(crate) fn build_block_title(
  input: TitleInputs<'_>,
  filter_chip_label: &str,
  palette: &Palette,
  pane_focused: bool,
) -> Line<'static> {
  // The full title strip including borders consumes the whole top
  // edge. ratatui leaves 1 cell on each side for the corner glyphs.
  // Reserve one cell each side for the leading/trailing space the
  // title carries inside the block edge.
  let budget = input.area_width.saturating_sub(4);

  // Local copy so we can drop entries under budget pressure without
  // mutating the caller's Vec.
  let mut hints: Vec<String> = input.hints.clone();

  // Filter slot text width (no styling here — we just need the cell
  // count for the budget calculation).
  let filter_text_width = match input.filter {
    FilterTitle::Inactive => filter_chip_label.chars().count(),
    FilterTitle::Active { buffer, focused } => {
      // `/ buffer` plus the cursor block when focused.
      "/ ".chars().count() + buffer.chars().count() + if focused { 1 } else { 0 }
    }
  };

  let count = format!("Models [{}]", input.total);
  // Trim hints from the tail until the line fits. The first hint
  // (caller-chosen, typically `Enter:launch` or `Enter:apply`) is
  // never dropped — agents and new users rely on it to bootstrap
  // the keyboard surface. Hint separator is ` · ` (3 cells).
  loop {
    let mut width = 1; // leading space
    width += count.chars().count();
    width += 3; // ` · ` before filter slot
    width += filter_text_width;
    for h in &hints {
      width += 3 + h.chars().count();
    }
    width += 1; // trailing space
    if width <= budget || hints.len() <= 1 {
      break;
    }
    hints.pop();
  }

  // Now build the actual Line with styled spans.
  let mut spans: Vec<Span<'static>> = Vec::with_capacity(8);
  spans.push(Span::raw(" "));
  // Underline the leading `M` of `Models` so it reads as a
  // press-this-letter shortcut (Shift+M re-focuses the list). When
  // the pane is unfocused, drop to `muted_style` so the heading
  // recedes — the active pane (right) wears the bold panel_title
  // tone. Matches the inactive-tab treatment in `right_pane`.
  let title_style = if pane_focused {
    palette.title_style()
  } else {
    palette.muted_style()
  };
  let mut count_chars = count.chars();
  match count_chars.next() {
    Some(first) => {
      spans.push(Span::styled(
        first.to_string(),
        title_style.add_modifier(Modifier::UNDERLINED),
      ));
      let rest: String = count_chars.collect();
      if !rest.is_empty() {
        spans.push(Span::styled(rest, title_style));
      }
    }
    None => spans.push(Span::styled(count, title_style)),
  }
  spans.push(Span::styled(" · ".to_string(), palette.muted_style()));

  // Filter slot. Inactive chip uses the same muted style as the
  // other hints so the title reads as a uniform hint strip.
  match input.filter {
    FilterTitle::Inactive => {
      spans.push(Span::styled(
        filter_chip_label.to_string(),
        palette.muted_style(),
      ));
    }
    FilterTitle::Active { buffer, focused } => {
      spans.push(Span::styled(
        "/ ".to_string(),
        Style::default()
          .fg(palette.accent)
          .add_modifier(Modifier::BOLD),
      ));
      spans.push(Span::styled(buffer.to_string(), palette.text_style()));
      if focused {
        spans.push(crate::tui::fmt::caret(palette));
      }
    }
  }

  // Hint chips, separated by ` · `.
  for h in hints {
    spans.push(Span::styled(" · ".to_string(), palette.muted_style()));
    spans.push(Span::styled(h, palette.muted_style()));
  }
  spans.push(Span::raw(" "));

  Line::from(spans)
}

/// Left-aligned pad/truncate to `w` display columns. Truncated
/// strings end with `…` so overflow is visible.
fn cell(s: &str, w: usize) -> String {
  if w == 0 {
    return String::new();
  }
  let count = s.chars().count();
  if count <= w {
    let mut out = String::with_capacity(w);
    out.push_str(s);
    for _ in count..w {
      out.push(' ');
    }
    out
  } else {
    let keep = w.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
  }
}

/// Foreground colour for a model row, encoding launch state into the
/// row colour so the whole strip reads as one semantic unit (matches
/// the glyph colour from `colour_for`). Active states paint:
///  - `Ready` → `success` (green)
///  - `Launching` / `Loading` → `status_loading` (yellow)
///  - `Error` → `error` (red)
///
/// Terminal states (`Stopped` / `External` / `NotLaunched`) fall back
/// to the default `fg` so the eye is drawn to live/changing rows
/// rather than rows that are just sitting there.
fn row_fg(state: SurfaceState, palette: &Palette) -> ratatui::style::Color {
  match state {
    SurfaceState::Ready => palette.success,
    SurfaceState::Launching | SurfaceState::Loading => palette.status_loading,
    SurfaceState::Error => palette.error,
    SurfaceState::NotLaunched | SurfaceState::Stopped | SurfaceState::External => palette.fg,
  }
}

/// Single combined marker for a model row. Priority winner from
/// highest to lowest:
///   1. Selection cursor `>` — wins on the focused row so the
///      selection is always unambiguous. No explicit fg so the
///      `Modifier::REVERSED` on the row flips the whole strip with
///      the row's semantic colour.
///   2. Launch-state glyph (`◌ ◐ ● ▲ ○ ⇪`) — wins whenever
///      [`glyph_for`] returns a non-space char (`NotLaunched` maps
///      to space, which falls through). Painted with
///      [`colour_for`] so a Ready row stays green, an Error row
///      stays red, etc.
///   3. Favorite star `★` — wins on idle favorited rows.
///   4. Blank — nothing to surface.
///
/// Always returns a 3-cell span (` X `) so the Name column lines
/// up across every row regardless of which slot won.
fn marker_span(
  state: SurfaceState,
  favorite: bool,
  is_selected: bool,
  palette: &Palette,
) -> Span<'static> {
  if is_selected {
    return Span::styled(
      " > ".to_string(),
      Style::default().add_modifier(Modifier::BOLD),
    );
  }
  let glyph = glyph_for(state);
  if glyph != ' ' {
    return Span::styled(
      format!(" {glyph} "),
      Style::default().fg(colour_for(state, palette)),
    );
  }
  if favorite {
    return Span::styled(" ★ ".to_string(), palette.warning_style());
  }
  Span::raw("   ".to_string())
}

/// Resolve the rendered value for one `(column, model-row)` pair.
/// Centralises the per-column extraction so the [`COLUMNS`] table
/// stays declarative and both the table-header and model-row paths
/// share one source of truth.
fn column_value(id: ColumnId, model: &ListRow) -> String {
  let ListRow::Model {
    arch,
    quant,
    native_ctx,
    weights_bytes,
    mode_hint,
    port,
    ..
  } = model
  else {
    return String::new();
  };
  match id {
    ColumnId::Arch => arch.clone(),
    ColumnId::Quant => quant.clone(),
    ColumnId::Ctx => native_ctx.map(format_tokens).unwrap_or_else(|| "—".into()),
    ColumnId::Size => weights_bytes
      .map(format_bytes)
      .unwrap_or_else(|| "—".into()),
    ColumnId::Mode => mode_hint.clone(),
    ColumnId::Port => port.map(|p| format!(":{p}")).unwrap_or_else(|| "—".into()),
  }
}

fn render_row<'a>(
  row: &'a ListRow,
  palette: &Palette,
  layout: &ColumnLayout,
  content_w: usize,
  is_selected: bool,
) -> ListItem<'a> {
  let name_w = layout.name_w;
  let cols = layout.visible.as_slice();
  match row {
    ListRow::TableHeader => {
      // Label cells line up with model-row value cells: same widths,
      // same separators, same marker gutter (rendered as blanks).
      // Header is bolded and tinted with the accent colour to set it
      // apart from group headers and model rows.
      let mut line = String::with_capacity(content_w);
      line.push_str(&" ".repeat(MARKER_W));
      line.push_str(&cell("Name", name_w));
      for c in cols {
        line.push(' ');
        line.push_str(&cell(c.label, c.width));
      }
      ListItem::new(Line::from(Span::styled(
        line,
        Style::default()
          .fg(palette.label)
          .add_modifier(Modifier::BOLD),
      )))
    }
    ListRow::Header { label } => {
      // Group header (favorites or folder path). Render across the
      // full content width so it visibly separates groups; the label
      // is ellipsised if the directory path is longer than the pane.
      // Painted with `palette.label` (blue per theme) so folder
      // names match the in-pane label convention rather than
      // disappearing into the muted hint tone.
      let shown = cell(label.as_str(), content_w);
      ListItem::new(Line::from(Span::styled(
        shown,
        Style::default()
          .fg(palette.label)
          .add_modifier(Modifier::BOLD),
      )))
    }
    ListRow::Divider => {
      // Thin horizontal rule painted across the full inner width in
      // the muted palette so it reads as ambient separation rather
      // than data. Drawn with `─` (U+2500) so it lines up with the
      // box-drawing border characters already on the block.
      ListItem::new(Line::from(Span::styled(
        "─".repeat(content_w),
        palette.muted_style(),
      )))
    }
    ListRow::Model {
      name,
      favorite,
      state,
      ..
    } => {
      // The whole row carries a single semantic foreground via
      // `ListItem::style`. Spans for name/columns/separators leave
      // their fg unset so they inherit the row's colour — that way
      // when the row gets `Modifier::REVERSED` on selection, every
      // cell flips with the same source colour and the row reads
      // as one inverted block instead of cell-by-cell splotches.
      // The marker keeps its semantic colour (state glyph / favorite
      // star) so the launch state stays scannable even on unselected
      // rows; the selection cursor leaves fg unset so REVERSED
      // flips it with the rest of the row.
      let fg = row_fg(*state, palette);
      let mut spans: Vec<Span<'a>> = Vec::with_capacity(2 + cols.len() * 2);
      spans.push(marker_span(*state, *favorite, is_selected, palette));
      spans.push(Span::raw(cell(name.as_str(), name_w)));
      for c in cols {
        spans.push(Span::raw(" "));
        spans.push(Span::raw(cell(&column_value(c.id, row), c.width)));
      }
      // `ListItem::style` is the canonical place to apply
      // `Modifier::REVERSED` to the *whole* row uniformly. Setting
      // fg here makes every unset-fg span (name + columns +
      // separator spaces) render in `fg`, so REVERSED flips the
      // whole strip in one go.
      ListItem::new(Line::from(spans)).style(Style::default().fg(fg))
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::discovery::ModelSource;
  use crate::gguf::metadata::{ModeHint, ModelMetadata, Quant};

  fn fake(path: &str, parent: &str) -> DiscoveredModel {
    DiscoveredModel {
      path: PathBuf::from(path),
      parent: PathBuf::from(parent),
      source: ModelSource::UserPath,
      metadata: Some(ModelMetadata {
        arch: Some("llama".into()),
        total_parameters: Some(7_000_000_000),
        parameter_label: Some("7B".into()),
        quant: Quant::Q4_K,
        native_ctx: Some(8192),
        chat_template: None,
        tokenizer_kind: None,
        reasoning_hint: false,
        mode_hint: ModeHint::Chat,
        weights_bytes: Some(4_200_000_000),
      }),
      parse_error: None,
      split_siblings: Vec::new(),
      display_label: None,
    }
  }

  #[test]
  fn build_rows_starts_with_a_table_header_when_models_present() {
    let states = BTreeMap::new();
    let m = fake("/m/a.gguf", "/m");
    let rows = build_rows(RowInputs {
      models: std::slice::from_ref(&m),
      favorites: &[],
      model_states: &states,
      model_ports: &BTreeMap::new(),
      running: &[],
      recent_paths: &[],
    });
    assert_eq!(rows.first(), Some(&ListRow::TableHeader));
  }

  #[test]
  fn build_rows_empty_input_yields_no_rows_so_empty_state_can_render() {
    // The renderer in `render.rs` flips to `render_empty_state` when
    // `rendered_rows()` is empty; returning `[TableHeader]` here
    // would mask that hint with a stray column-label row.
    let states = BTreeMap::new();
    let rows = build_rows(RowInputs {
      models: &[],
      favorites: &[],
      model_states: &states,
      model_ports: &BTreeMap::new(),
      running: &[],
      recent_paths: &[],
    });
    assert!(rows.is_empty(), "no models → no rows at all");
  }

  #[test]
  fn table_header_is_not_selectable_and_carries_no_path() {
    let h = ListRow::TableHeader;
    assert!(!h.is_selectable(), "table header is a label, not a target");
    assert!(h.path().is_none());
  }

  #[test]
  fn favorites_section_only_appears_when_favorites_present() {
    let m = fake("/m/a.gguf", "/m");
    let states = BTreeMap::new();
    let rows = build_rows(RowInputs {
      models: std::slice::from_ref(&m),
      favorites: &[],
      model_states: &states,
      model_ports: &BTreeMap::new(),
      running: &[],
      recent_paths: &[],
    });
    assert!(
      !rows
        .iter()
        .any(|r| matches!(r, ListRow::Header { label } if label.contains("Favorites"))),
      "no favorites = no favorites header"
    );
  }

  #[test]
  fn favorites_appear_first_below_table_header() {
    let a = fake("/m/a.gguf", "/m");
    let b = fake("/m/b.gguf", "/m");
    let states = BTreeMap::new();
    let rows = build_rows(RowInputs {
      models: &[a.clone(), b.clone()],
      favorites: std::slice::from_ref(&a.path),
      model_states: &states,
      model_ports: &BTreeMap::new(),
      running: &[],
      recent_paths: &[],
    });
    // Row 0 is the table header; row 1 should be the favorites group.
    assert_eq!(rows[0], ListRow::TableHeader);
    let first_group = match &rows[1] {
      ListRow::Header { label } => label.clone(),
      _ => panic!("expected group header at row 1, got {:?}", rows[1]),
    };
    assert!(
      first_group.contains("Favorites"),
      "favorites header sits directly below the table header, got: {first_group}"
    );
    // The favorited model is row 2.
    let first_model = match &rows[2] {
      ListRow::Model { path, .. } => path.clone(),
      _ => panic!("expected model row at index 2"),
    };
    assert_eq!(first_model, a.path);
  }

  #[test]
  fn display_name_returns_full_file_stem() {
    let m = fake(
      "/hf/models/Qwen2.5-Coder-7B-Instruct-Q4_K_M.gguf",
      "/home/alice/.cache/huggingface/hub/models--bartowski--Qwen2.5-Coder-7B-Instruct-GGUF/snapshots/1234",
    );
    assert_eq!(display_name(&m), "Qwen2.5-Coder-7B-Instruct-Q4_K_M");
  }

  #[test]
  fn build_rows_places_running_section_at_top_with_per_launch_rows() {
    // Two launches of the same model should produce two Running
    // rows, each carrying its own `launch_id` and port. The section
    // header sits above them so the user can scan kdash-style.
    let m = fake("/m/a.gguf", "/m");
    let running = vec![
      RunningLaunchRow {
        launch_id: "L2".into(),
        path: m.path.clone(),
        port: 41101,
        state: SurfaceState::Ready,
      },
      RunningLaunchRow {
        launch_id: "L1".into(),
        path: m.path.clone(),
        port: 41100,
        state: SurfaceState::Ready,
      },
    ];
    let rows = build_rows(RowInputs {
      models: std::slice::from_ref(&m),
      favorites: &[],
      model_states: &BTreeMap::new(),
      model_ports: &BTreeMap::new(),
      running: &running,
      recent_paths: &[],
    });
    // Expect: [TableHeader, Header(▶ Running), Model(L2), Model(L1), Header(/m), Model(catalog)]
    assert_eq!(rows.first(), Some(&ListRow::TableHeader));
    let running_header = rows.iter().find_map(|r| match r {
      ListRow::Header { label } if label.contains("Running") => Some(label.clone()),
      _ => None,
    });
    assert!(running_header.is_some(), "Running header must appear");
    let launch_ids: Vec<String> = rows
      .iter()
      .filter_map(|r| match r {
        ListRow::Model {
          launch_id: Some(id),
          ..
        } => Some(id.clone()),
        _ => None,
      })
      .collect();
    assert_eq!(
      launch_ids,
      vec!["L2".to_string(), "L1".to_string()],
      "duplicate launches surface as separate rows in caller order"
    );
  }

  #[test]
  fn build_rows_recent_section_skips_paths_currently_running() {
    // The Recent group shouldn't duplicate rows that already
    // appear in the Running section — that would clutter the list
    // and mislead the user about how many instances are live.
    let a = fake("/m/a.gguf", "/m");
    let b = fake("/m/b.gguf", "/m");
    let running = vec![RunningLaunchRow {
      launch_id: "L1".into(),
      path: a.path.clone(),
      port: 41100,
      state: SurfaceState::Ready,
    }];
    let recent = vec![a.path.clone(), b.path.clone()];
    let rows = build_rows(RowInputs {
      models: &[a.clone(), b.clone()],
      favorites: &[],
      model_states: &BTreeMap::new(),
      model_ports: &BTreeMap::new(),
      running: &running,
      recent_paths: &recent,
    });
    let recent_section_idx = rows.iter().position(|r| match r {
      ListRow::Header { label } => label.contains("Recent"),
      _ => false,
    });
    let recent_idx = recent_section_idx.expect("Recent section must appear");
    // Rows immediately after the Recent header — until the next
    // Header — are the section's contents.
    let section_paths: Vec<&PathBuf> = rows[recent_idx + 1..]
      .iter()
      .take_while(|r| !matches!(r, ListRow::Header { .. }))
      .filter_map(|r| match r {
        ListRow::Model { path, .. } => Some(path),
        _ => None,
      })
      .collect();
    assert_eq!(
      section_paths,
      vec![&b.path],
      "Recent must skip paths currently in Running"
    );
  }

  #[test]
  fn build_rows_attaches_port_to_running_model_row() {
    // When the daemon reports a path with a bound port, the
    // corresponding Model row carries it in the new `port` field so
    // the Port column can render `:port` instead of `—`.
    let m = fake("/m/a.gguf", "/m");
    let states = BTreeMap::new();
    let mut ports: BTreeMap<PathBuf, u16> = BTreeMap::new();
    ports.insert(m.path.clone(), 41100);
    let rows = build_rows(RowInputs {
      models: std::slice::from_ref(&m),
      favorites: &[],
      model_states: &states,
      model_ports: &ports,
      running: &[],
      recent_paths: &[],
    });
    let row_port = rows.iter().find_map(|r| match r {
      ListRow::Model { port, .. } => Some(*port),
      _ => None,
    });
    assert_eq!(row_port, Some(Some(41100)));
  }

  #[test]
  fn build_rows_leaves_port_unset_for_paths_without_a_launch() {
    // Discovered-but-not-running paths get `port: None` so the
    // Port column renders the `—` glyph instead of a stale port
    // from a previous session.
    let m = fake("/m/a.gguf", "/m");
    let states = BTreeMap::new();
    let rows = build_rows(RowInputs {
      models: std::slice::from_ref(&m),
      favorites: &[],
      model_states: &states,
      model_ports: &BTreeMap::new(),
      running: &[],
      recent_paths: &[],
    });
    let row_port = rows.iter().find_map(|r| match r {
      ListRow::Model { port, .. } => Some(*port),
      _ => None,
    });
    assert_eq!(row_port, Some(None));
  }

  #[test]
  fn favorited_model_appears_in_both_favorites_and_its_folder_group() {
    // Running paths drop from the catalog groupings, but favorited
    // paths *don't* — the user expects to find their model in its
    // original folder, with the `★ Favorites` section just acting as
    // a shortcut.
    let a = fake("/m/x/a.gguf", "/m/x");
    let b = fake("/m/x/b.gguf", "/m/x");
    let rows = build_rows(RowInputs {
      models: &[a.clone(), b.clone()],
      favorites: std::slice::from_ref(&a.path),
      model_states: &BTreeMap::new(),
      model_ports: &BTreeMap::new(),
      running: &[],
      recent_paths: &[],
    });
    let a_rows = rows
      .iter()
      .filter(|r| matches!(r, ListRow::Model { path, .. } if path == &a.path))
      .count();
    assert_eq!(
      a_rows, 2,
      "favorited model must surface in both Favorites and its folder, got {a_rows} rows"
    );
    // The folder copy must still wear the favorite star so the user
    // doesn't lose the favorited signal when scanning by folder.
    let folder_copy_is_favorite = rows.iter().any(|r| {
      matches!(
        r,
        ListRow::Model { path, favorite: true, .. } if path == &a.path
      )
    });
    assert!(
      folder_copy_is_favorite,
      "favorite star must persist in the folder group"
    );
  }

  #[test]
  fn divider_separates_favorites_from_folder_groups() {
    // A row layout with both Favorites and folder groups must carry
    // a `Divider` between them so the eye can tell the shortcut
    // section apart from the original-folder section (favorited rows
    // appear in both now).
    let a = fake("/m/x/a.gguf", "/m/x");
    let b = fake("/m/y/b.gguf", "/m/y");
    let rows = build_rows(RowInputs {
      models: &[a.clone(), b.clone()],
      favorites: std::slice::from_ref(&a.path),
      model_states: &BTreeMap::new(),
      model_ports: &BTreeMap::new(),
      running: &[],
      recent_paths: &[],
    });
    let divider_idx = rows
      .iter()
      .position(|r| matches!(r, ListRow::Divider))
      .expect("Divider must appear between Favorites and folder groups");
    // Whatever sits immediately before the divider must be a Model
    // row (the last favorite); whatever sits immediately after must
    // be a Header (the first folder group). That's how we know the
    // divider's neighbours are the two sections it's separating.
    assert!(
      matches!(rows[divider_idx - 1], ListRow::Model { .. }),
      "row before divider must be the last Favorites entry"
    );
    assert!(
      matches!(rows[divider_idx + 1], ListRow::Header { .. }),
      "row after divider must be the first folder header"
    );
  }

  #[test]
  fn divider_is_omitted_when_no_folder_groups_follow_favorites() {
    // Edge case: every catalog entry is a favorite, so there are no
    // folder groups left to separate from. A trailing divider with
    // nothing under it would just clutter the bottom of the list.
    let a = fake("/m/x/a.gguf", "/m/x");
    let rows = build_rows(RowInputs {
      models: std::slice::from_ref(&a),
      favorites: std::slice::from_ref(&a.path),
      model_states: &BTreeMap::new(),
      model_ports: &BTreeMap::new(),
      running: &[],
      recent_paths: &[],
    });
    // No divider expected since the favorited row also fills the
    // /m/x folder slot — meaning the folder group does exist.
    // Re-run with a running launch so the path drops from the
    // folder group entirely, leaving Favorites as the sole section.
    let running = vec![RunningLaunchRow {
      launch_id: "L1".into(),
      path: a.path.clone(),
      port: 41100,
      state: SurfaceState::Ready,
    }];
    let rows_with_running = build_rows(RowInputs {
      models: std::slice::from_ref(&a),
      favorites: std::slice::from_ref(&a.path),
      model_states: &BTreeMap::new(),
      model_ports: &BTreeMap::new(),
      running: &running,
      recent_paths: &[],
    });
    // First Vec carries Favorites + /m/x folder → divider expected.
    assert!(
      rows.iter().any(|r| matches!(r, ListRow::Divider)),
      "divider expected when /m/x folder section still emits"
    );
    // Second Vec — running drops the path from the folder, leaving
    // no folder groups → no divider.
    assert!(
      !rows_with_running
        .iter()
        .any(|r| matches!(r, ListRow::Divider)),
      "divider must drop when no folder section follows"
    );
  }

  #[test]
  fn divider_is_not_selectable_and_carries_no_path() {
    let d = ListRow::Divider;
    assert!(!d.is_selectable(), "divider is decoration, not a target");
    assert!(d.path().is_none(), "divider has no associated path");
  }

  #[test]
  fn directory_groups_render_sorted_by_parent() {
    let a = fake("/m/x/a.gguf", "/m/x");
    let b = fake("/m/y/b.gguf", "/m/y");
    let states = BTreeMap::new();
    let rows = build_rows(RowInputs {
      models: &[a, b],
      favorites: &[],
      model_states: &states,
      model_ports: &BTreeMap::new(),
      running: &[],
      recent_paths: &[],
    });
    let headers: Vec<String> = rows
      .iter()
      .filter_map(|r| match r {
        ListRow::Header { label } => Some(label.clone()),
        _ => None,
      })
      .collect();
    assert_eq!(headers, vec!["m/x".to_string(), "m/y".to_string()]);
  }

  fn title_text(line: &ratatui::text::Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
  }

  fn macchiato() -> &'static crate::theme::Palette {
    crate::theme::palette_for(crate::theme::ThemeName::Macchiato)
  }

  /// Mirrors the chip list `render.rs::build_models_hints` produces
  /// for the running-row, filter-inactive case. Kept inline here so
  /// the in-module tests can exercise `build_block_title` directly
  /// without spinning an App.
  fn full_hints() -> Vec<String> {
    vec![
      "Enter:launch".into(),
      "s:stop".into(),
      "f:fav".into(),
      "p:path".into(),
      "u:url".into(),
      "c:curl".into(),
    ]
  }

  fn filter_hints() -> Vec<String> {
    vec!["Enter:apply".into(), "Esc:clear".into()]
  }

  #[test]
  fn title_filter_hint_renders_before_other_hints() {
    let title = build_block_title(
      TitleInputs {
        total: 127,
        area_width: 120,
        filter: FilterTitle::Inactive,
        hints: full_hints(),
      },
      "/:filter",
      macchiato(),
      true,
    );
    let text = title_text(&title);
    let filter_at = text.find("/:filter").expect("filter chip");
    let enter_at = text.find("Enter:launch").expect("enter chip");
    assert!(
      filter_at < enter_at,
      "/:filter must come before Enter:launch: {text:?}"
    );
  }

  #[test]
  fn title_includes_count_and_full_hint_strip_when_filter_inactive() {
    let title = build_block_title(
      TitleInputs {
        total: 127,
        area_width: 200,
        filter: FilterTitle::Inactive,
        hints: full_hints(),
      },
      "/:filter",
      macchiato(),
      true,
    );
    let text = title_text(&title);
    assert!(text.contains("Models [127]"));
    assert!(text.contains("Enter:launch"));
    assert!(text.contains("/:filter"));
    assert!(text.contains("s:stop"));
    assert!(text.contains("f:fav"));
    assert!(text.contains("p:path"));
    assert!(text.contains("u:url"));
    assert!(text.contains("c:curl"));
  }

  #[test]
  fn title_filter_active_shows_only_enter_apply_and_esc_clear_hints() {
    // While the user is typing into the filter, the row-action hint
    // strip collapses to just Enter:apply (commit + return focus to
    // the list) and Esc:clear (drop the buffer + close), so the
    // title doesn't distract from the in-flight query.
    let title = build_block_title(
      TitleInputs {
        total: 127,
        area_width: 200,
        filter: FilterTitle::Active {
          buffer: "qwen",
          focused: true,
        },
        hints: filter_hints(),
      },
      "/:filter",
      macchiato(),
      true,
    );
    let text = title_text(&title);
    assert!(
      text.contains("Enter:apply"),
      "expected Enter:apply: {text:?}"
    );
    assert!(text.contains("Esc:clear"), "expected Esc:clear: {text:?}");
    for missing in [
      "Enter:launch",
      "f:fav",
      "p:path",
      "s:stop",
      "u:url",
      "c:curl",
    ] {
      assert!(
        !text.contains(missing),
        "filter-active strip must drop `{missing}`: {text:?}"
      );
    }
  }

  #[test]
  fn title_renders_inline_filter_input_when_filter_active() {
    let title = build_block_title(
      TitleInputs {
        total: 127,
        area_width: 120,
        filter: FilterTitle::Active {
          buffer: "qwen",
          focused: true,
        },
        hints: filter_hints(),
      },
      "/:filter",
      macchiato(),
      true,
    );
    let text = title_text(&title);
    assert!(
      text.contains("/ qwen"),
      "expected inline `/ qwen` input, got: {text:?}"
    );
    assert!(
      !text.contains("/:filter"),
      "inline input replaces the `/:filter` hint: {text:?}"
    );
    // The `Esc:clear` hint replaces the row-action chips while
    // filter input is active.
    assert!(text.contains("Esc:clear"));
  }

  #[test]
  fn title_inline_input_carries_cursor_block_when_focused() {
    let title = build_block_title(
      TitleInputs {
        total: 127,
        area_width: 120,
        filter: FilterTitle::Active {
          buffer: "q",
          focused: true,
        },
        hints: filter_hints(),
      },
      "/:filter",
      macchiato(),
      true,
    );
    let text = title_text(&title);
    // The cursor span is a single block glyph appended after the
    // buffer; the exact char is `▏` (U+258F) — assert any non-ASCII
    // block char follows the buffer.
    let after = text.split("/ q").nth(1).expect("split after buffer");
    assert!(
      after.chars().next().map(|c| !c.is_ascii()).unwrap_or(false),
      "expected a cursor glyph after the buffer, got: {after:?}"
    );
  }

  #[test]
  fn title_omits_running_row_hints_when_caller_drops_them() {
    // The renderer in `render.rs::build_models_hints` already omits
    // the running-row trio when the cursor sits on an unlaunched
    // model; the title builder just renders whatever it's handed.
    let title = build_block_title(
      TitleInputs {
        total: 3,
        area_width: 120,
        filter: FilterTitle::Inactive,
        hints: vec!["Enter:launch".into(), "f:fav".into(), "p:path".into()],
      },
      "/:filter",
      macchiato(),
      true,
    );
    let text = title_text(&title);
    assert!(
      !text.contains("s:stop"),
      "s:stop must hide when caller drops it: {text:?}"
    );
    assert!(text.contains("Enter:launch"));
    assert!(text.contains("f:fav"));
  }

  #[test]
  fn title_drops_hints_right_to_left_under_pressure() {
    // A 40-col area can't fit the whole strip; the title builder
    // should drop hints from the tail. With the chip order the
    // caller supplies, the yank pair (u:url · c:curl) sits at the
    // tail and is the first to go. `s:stop` now sits near the head
    // so it survives — that's the point of the reorder.
    let title = build_block_title(
      TitleInputs {
        total: 127,
        area_width: 40,
        filter: FilterTitle::Inactive,
        hints: full_hints(),
      },
      "/:filter",
      macchiato(),
      true,
    );
    let text = title_text(&title);
    assert!(
      text.contains("Enter:launch"),
      "must never drop launch chip: {text:?}"
    );
    assert!(
      text.contains("Models [127]"),
      "must never drop the count: {text:?}"
    );
    // The running-row hints sit at the tail and should drop first.
    assert!(
      !text.contains("c:curl"),
      "expected c:curl dropped at 40 cols: {text:?}"
    );
  }

  #[test]
  fn surface_state_overlay_picks_up_running_supervisor_state() {
    let m = fake("/m/a.gguf", "/m");
    let mut states = BTreeMap::new();
    states.insert(m.path.clone(), SurfaceState::Ready);
    let rows = build_rows(RowInputs {
      models: std::slice::from_ref(&m),
      favorites: &[],
      model_states: &states,
      model_ports: &BTreeMap::new(),
      running: &[],
      recent_paths: &[],
    });
    let model_row = rows
      .iter()
      .find_map(|r| match r {
        ListRow::Model { state, .. } => Some(*state),
        _ => None,
      })
      .expect("model row");
    assert_eq!(model_row, SurfaceState::Ready);
  }

  #[test]
  fn cell_pads_short_values_to_width() {
    assert_eq!(cell("hi", 5), "hi   ");
  }

  #[test]
  fn cell_truncates_with_ellipsis_when_value_overflows() {
    // 5 cells: "abcde" → "abcd…"
    assert_eq!(cell("abcdef", 5), "abcd…");
  }

  #[test]
  fn cell_width_zero_yields_empty_string() {
    assert_eq!(cell("anything", 0), "");
  }

  #[test]
  fn row_fg_paints_whole_row_per_lifecycle_state() {
    // Active states (Ready / Launching / Loading / Error) paint the
    // whole row in their semantic colour so the strip reads as one
    // unit alongside the status glyph. Terminal states stay in `fg`
    // so the eye isn't drawn to rows that aren't doing anything.
    use crate::theme::{palette_for, ThemeName};
    let p = palette_for(ThemeName::Macchiato);
    assert_eq!(row_fg(SurfaceState::Ready, p), p.success);
    assert_eq!(row_fg(SurfaceState::Error, p), p.error);
    assert_eq!(
      row_fg(SurfaceState::Launching, p),
      p.status_loading,
      "starting rows should paint yellow so the user sees activity"
    );
    assert_eq!(
      row_fg(SurfaceState::Loading, p),
      p.status_loading,
      "model-load rows should match the launching colour"
    );
    // Terminal / inactive states drop back to the default fg.
    assert_eq!(row_fg(SurfaceState::NotLaunched, p), p.fg);
    assert_eq!(row_fg(SurfaceState::Stopped, p), p.fg);
    assert_eq!(row_fg(SurfaceState::External, p), p.fg);
  }

  /// Total cost (cells) of every declared column, including its
  /// leading separator. Pulled out of the inline tests so the
  /// ranked picker (and any future budget calculation) can verify
  /// against the same source of truth.
  fn all_columns_cost() -> usize {
    COLUMNS.iter().map(|c| c.width + COL_SEP_W).sum()
  }

  #[test]
  fn layout_picks_every_column_when_pane_is_wide() {
    // Pane wide enough for every data column plus generous Name.
    let layout = layout_columns(200);
    assert_eq!(
      layout.visible.len(),
      COLUMNS.len(),
      "all columns must fit at 200 cells"
    );
    assert_eq!(layout.name_w, 200 - MARKER_W - all_columns_cost());
  }

  #[test]
  fn layout_drops_no_columns_when_only_zero_budget_remains() {
    // content_w = chrome + MIN_NAME_W → budget == 0. Nothing fits.
    // Name parks exactly at MIN_NAME_W.
    let layout = layout_columns(MARKER_W + MIN_NAME_W);
    assert!(layout.visible.is_empty());
    assert_eq!(layout.name_w, MIN_NAME_W);
  }

  #[test]
  fn layout_keeps_lowest_rank_column_when_only_one_fits() {
    // Budget == one Size column (rank 10, cost 6+1=7). Name still
    // at MIN_NAME_W.
    let layout = layout_columns(MARKER_W + MIN_NAME_W + 7);
    assert_eq!(layout.visible.len(), 1);
    assert_eq!(layout.visible[0].id, ColumnId::Size);
    assert_eq!(layout.name_w, MIN_NAME_W);
  }

  #[test]
  fn layout_preserves_declaration_order_when_high_rank_columns_drop() {
    // Budget = 32 cells (= Arch+Quant+Ctx+Size cost). Mode (11) and
    // Port (7) don't both fit, but neither does either alone after
    // the rank-ordered greedy takes the cheaper ones first.
    let layout = layout_columns(MARKER_W + MIN_NAME_W + 32);
    let ids: Vec<ColumnId> = layout.visible.iter().map(|c| c.id).collect();
    // Source order preserved — Size (best rank) does not jump to
    // the front of the strip.
    assert_eq!(
      ids,
      vec![ColumnId::Arch, ColumnId::Quant, ColumnId::Ctx, ColumnId::Size]
    );
  }

  #[test]
  fn layout_can_keep_lowest_priority_column_if_a_higher_one_overflows() {
    // Budget = 39 cells. Greedy by rank takes Size+Quant+Ctx+Arch
    // (32), can't fit Mode (11 would put it over), but still fits
    // Port (7) at the tail. The visible set is non-contiguous in
    // declaration order — Port renders right after Size with no
    // Mode column between them.
    let layout = layout_columns(MARKER_W + MIN_NAME_W + 39);
    let ids: Vec<ColumnId> = layout.visible.iter().map(|c| c.id).collect();
    assert_eq!(
      ids,
      vec![
        ColumnId::Arch,
        ColumnId::Quant,
        ColumnId::Ctx,
        ColumnId::Size,
        ColumnId::Port
      ]
    );
  }

  #[test]
  fn models_title_underlines_leading_m_as_shift_jump_mnemonic() {
    // Shift+M re-focuses the model list. The leading `M` of
    // `Models [N]` carries the UNDERLINED modifier on top of the
    // panel-title bold so it reads as a press-this-letter hint.
    use crate::theme::{palette_for, ThemeName};
    let palette = palette_for(ThemeName::Macchiato);
    let line = build_block_title(
      TitleInputs {
        area_width: 80,
        total: 3,
        filter: FilterTitle::Inactive,
        hints: vec!["Enter:launch".to_string()],
      },
      ":filter",
      palette,
      true,
    );
    let m_span = line
      .spans
      .iter()
      .find(|s| s.content.as_ref() == "M")
      .expect("leading M span present in title");
    assert!(
      m_span.style.add_modifier.contains(Modifier::UNDERLINED),
      "leading M must be underlined as a mnemonic"
    );
  }

  #[test]
  fn models_title_drops_to_muted_when_pane_is_not_focused() {
    // When the right pane owns focus, the `Models [N]` heading
    // should recede to muted (no bold panel_title) so the active
    // pane carries the visual weight. Mirrors right_pane's
    // inactive-tab treatment.
    use crate::theme::{palette_for, ThemeName};
    let palette = palette_for(ThemeName::Macchiato);
    let inputs = || TitleInputs {
      area_width: 80,
      total: 3,
      filter: FilterTitle::Inactive,
      hints: vec!["Enter:launch".to_string()],
    };
    let focused = build_block_title(inputs(), "/:filter", palette, true);
    let unfocused = build_block_title(inputs(), "/:filter", palette, false);
    let m_focused = focused
      .spans
      .iter()
      .find(|s| s.content.as_ref() == "M")
      .expect("leading M span");
    let m_unfocused = unfocused
      .spans
      .iter()
      .find(|s| s.content.as_ref() == "M")
      .expect("leading M span");
    assert_eq!(m_focused.style.fg, Some(palette.panel_title));
    assert!(m_focused.style.add_modifier.contains(Modifier::BOLD));
    assert_eq!(
      m_unfocused.style.fg,
      Some(palette.muted),
      "unfocused title must paint with muted fg"
    );
    assert!(
      !m_unfocused.style.add_modifier.contains(Modifier::BOLD),
      "unfocused title must drop the bold modifier"
    );
    // The mnemonic underline survives both states so Shift+M still
    // reads as a press-this-letter hint.
    assert!(m_unfocused
      .style
      .add_modifier
      .contains(Modifier::UNDERLINED));
  }

  #[test]
  fn highlight_style_uses_reversed_so_selection_inverts_row_fg() {
    // KDash-style: the highlight is a pure REVERSED modifier, so a
    // Ready (green) row paints a green bar on selection, an Error
    // (red) row paints red, and normal rows paint with `palette.fg`.
    // No concrete bg/fg colours are pinned — that's what makes the
    // selection adopt the row's semantic colour uniformly.
    use crate::theme::{palette_for, ThemeName};
    for theme in [ThemeName::Macchiato, ThemeName::Latte, ThemeName::Mono] {
      let p = palette_for(theme);
      let style = highlight_style(p);
      assert_eq!(style.bg, None, "{theme:?} must not pin a bg colour");
      assert_eq!(style.fg, None, "{theme:?} must not pin a fg colour");
      assert!(style.add_modifier.contains(Modifier::REVERSED));
      assert!(style.add_modifier.contains(Modifier::BOLD));
    }
  }
}
