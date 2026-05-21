//! Launch picker form state — the typed-knob editor.
//!
//! The Settings tab renders a vertical list of rows: every
//! `TypedKnobs` field (ctx, reasoning, n_gpu_layers, … in
//! `knob_specs()` order) with a per-row source label (`(user)`,
//! `(last used)`, `(arch default)`, `(model default)`,
//! `(server default)`), plus an `extras` free-text row at the
//! bottom. Up/Down moves between rows; Left/Right cycles the focused
//! row's value; `e` enters inline edit; Enter launches (or commits
//! an open edit); Backspace resets the focused row.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::LazyLock;

use crate::config::TypedKnobs;
use crate::launch::flag_aliases::{KnobField, KV_CACHE_TYPES};
use crate::launch::params::LayerLabel;

/// Pre-canned context-length presets surfaced as quick picks. Custom
/// values flow through the same field when the user types digits.
pub const CTX_PRESETS: &[u32] = &[2048, 4096, 8192, 16384, 32768, 65536, 131072];

/// Which row the cursor is on. The editor renders top-to-bottom in
/// [`PickerField::all`] order so it doubles as the vertical-navigation
/// order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerField {
  Knob(KnobField),
  Extras,
}

/// Lazily-built navigation order — every knob in `knob_specs()`
/// order (ctx, reasoning, n_gpu_layers, …), followed by `Extras`.
/// Built once on first access so per-keypress navigation does no
/// allocation.
static ALL_FIELDS: LazyLock<Box<[PickerField]>> = LazyLock::new(|| {
  let mut v: Vec<PickerField> = Vec::new();
  for spec in crate::launch::flag_aliases::knob_specs() {
    v.push(PickerField::Knob(spec.field));
  }
  v.push(PickerField::Extras);
  v.into_boxed_slice()
});

impl PickerField {
  /// All rows in render / navigation order. Returns a static slice so
  /// `next_field` / `prev_field` don't allocate on each keypress.
  pub fn all() -> &'static [PickerField] {
    &ALL_FIELDS
  }

  /// Whether `e:edit` opens an inline buffer on this row.
  ///
  /// - Numeric / float / enum knobs and the free-text `Extras` row
  ///   open an `InputField` for typing.
  /// - Boolean knobs (reasoning, flash_attn, mlock, no_mmap) don't —
  ///   they're cycled with ←/→. Surfacing `e:edit` on a boolean row
  ///   would be a no-op chip and a misleading affordance.
  ///
  /// Shared between [`crate::tui::events::open_focused_inline_edit`]
  /// (which early-returns on booleans) and the right-pane hint strip
  /// (which hides the chip on those rows) so the chip and the
  /// handler stay in lockstep.
  pub fn is_editable(self) -> bool {
    match self {
      PickerField::Extras => true,
      PickerField::Knob(k) => match k {
        KnobField::Reasoning | KnobField::FlashAttn | KnobField::Mlock | KnobField::NoMmap => false,
        KnobField::Ctx
        | KnobField::NGpuLayers
        | KnobField::Threads
        | KnobField::Parallel
        | KnobField::BatchSize
        | KnobField::UbatchSize
        | KnobField::Keep
        | KnobField::RopeFreqScale
        | KnobField::CacheTypeK
        | KnobField::CacheTypeV => true,
      },
    }
  }
}

/// Inline-edit state owned by [`LaunchPickerState`].
///
/// The buffer and modal `editing` flag live in `inline_edit`
/// ([`InputField`]) so the typed-knob editor shares the
/// `e:edit / Esc:walk-back / Enter:Submit` contract with every
/// other text input in the TUI. The wrapper carries the two extra
/// pieces of state `InputField` doesn't model:
///
/// - `field` — which `PickerField` the open edit is editing (numeric
///   / enum knob or the extras row), so `commit_inline_edit` knows
///   where to write the parsed value.
/// - `error` — the inline parse / validation error rendered under
///   the row when commit fails.
///
/// Both reset when the edit closes (either via successful commit
/// or `Esc` walk-back).
#[derive(Debug, Clone, Default)]
pub struct InlineEdit {
  pub field: Option<PickerField>,
  pub input: crate::tui::input_field::InputField,
  pub error: Option<String>,
}

impl InlineEdit {
  /// Open the edit on `field`, seed the buffer with `initial`, and
  /// enter edit mode so subsequent keystrokes append to the buffer.
  pub fn open(&mut self, field: PickerField, initial: String) {
    self.field = Some(field);
    self.input.set_text(initial);
    self.input.enter_edit();
    self.error = None;
  }

  /// Close the edit — clear the field marker, drop the buffer, exit
  /// edit mode, and clear any stale error.
  pub fn close(&mut self) {
    self.field = None;
    self.input.clear();
    self.input.exit_edit();
    self.error = None;
  }

  /// True while the user is actively typing into the buffer (the
  /// edit is open *and* `InputField` reports edit mode). Used by
  /// the event router to send keys to the input instead of the
  /// outer keymap.
  pub fn is_open(&self) -> bool {
    self.field.is_some() && self.input.is_editing()
  }
}

/// State of the launch picker.
#[derive(Debug, Clone)]
pub struct LaunchPickerState {
  /// Display name of the focused model (rendered in the title).
  pub model_name: String,
  /// User-supplied typed knobs (only fields the user explicitly set;
  /// every other field stays `None` and inherits from the resolved
  /// chain on render). Includes `ctx` and `reasoning`.
  pub user_knobs: TypedKnobs,
  /// Resolved knobs after applying the layered resolver — what the
  /// editor shows for each row.
  pub resolved: TypedKnobs,
  /// Per-knob source labels for the right-aligned origin chip.
  pub sources: BTreeMap<KnobField, LayerLabel>,
  /// Free-form argv tail forwarded to llama-server.
  pub extras: Vec<std::ffi::OsString>,
  /// Modal text-input for the extras row (`is_editing()` replaces
  /// the bespoke `extras_editing` bool; `buffer()` replaces the raw
  /// string + cursor pair). Shares the `e:edit / Esc:walk-back /
  /// Enter:Submit` contract with every other text input in the TUI.
  pub extras_input: crate::tui::input_field::InputField,
  /// Inline edit state for numeric / enum rows. Wraps an
  /// [`InputField`] plus the `PickerField` marker so the commit path
  /// knows which row to write back to, and an optional parse-error
  /// string rendered under the row.
  pub inline_edit: InlineEdit,
  pub field: PickerField,
  pub active_instances: usize,
  pub prefer_port: Option<u16>,
  /// Row offset clipped from the top of the rendered line list so the
  /// focused row stays visible on small viewports. Recomputed on each
  /// render using the actual area height — the `Cell` lets the
  /// read-only render path (which only has `&App`) update the cached
  /// offset without taking a mutable borrow.
  pub scroll_offset: Cell<u16>,
}

impl LaunchPickerState {
  pub fn for_model(model_name: impl Into<String>) -> Self {
    Self {
      model_name: model_name.into(),
      user_knobs: TypedKnobs::default(),
      resolved: TypedKnobs::default(),
      sources: BTreeMap::new(),
      extras: Vec::new(),
      extras_input: crate::tui::input_field::InputField::default(),
      inline_edit: InlineEdit::default(),
      field: PickerField::Knob(KnobField::Ctx),
      active_instances: 0,
      prefer_port: None,
      scroll_offset: Cell::new(0),
    }
  }

  /// Seed the resolved knobs + source map from the layered resolver
  /// output. The user-knobs layer is empty on a freshly-opened
  /// editor — the rows show inherited values.
  pub fn set_resolved(&mut self, resolved: TypedKnobs, sources: BTreeMap<KnobField, LayerLabel>) {
    self.resolved = resolved;
    self.sources = sources;
  }

  /// Cycle the focused field's value forward (Right arrow).
  pub fn cycle_focused_value_next(&mut self) {
    match self.field {
      PickerField::Knob(k) => self.cycle_knob(k, true),
      PickerField::Extras => {}
    }
  }

  /// Cycle the focused field's value backward (Left arrow).
  pub fn cycle_focused_value_prev(&mut self) {
    match self.field {
      PickerField::Knob(k) => self.cycle_knob(k, false),
      PickerField::Extras => {}
    }
  }

  fn cycle_knob(&mut self, field: KnobField, forward: bool) {
    match field {
      KnobField::Ctx => self.cycle_u32(field, CTX_PRESETS, forward),
      KnobField::Reasoning => self.cycle_bool(field, forward),
      KnobField::NGpuLayers => self.cycle_u32(field, &[0, 16, 32, 64, 99], forward),
      KnobField::Threads => self.cycle_u32(field, &[1, 2, 4, 6, 8, 12, 16, 24], forward),
      KnobField::Parallel => self.cycle_u32(field, &[1, 2, 4, 8, 16], forward),
      KnobField::BatchSize => self.cycle_u32(field, &[256, 512, 1024, 2048, 4096], forward),
      KnobField::UbatchSize => self.cycle_u32(field, &[128, 256, 512, 1024], forward),
      KnobField::Keep => self.cycle_u32(field, &[0, 64, 128, 256, 512, 1024], forward),
      KnobField::RopeFreqScale => self.cycle_f32(field, &[0.5, 1.0, 2.0, 4.0], forward),
      KnobField::CacheTypeK | KnobField::CacheTypeV => self.cycle_enum(field, forward),
      KnobField::FlashAttn | KnobField::Mlock | KnobField::NoMmap => {
        self.cycle_bool(field, forward)
      }
    }
  }

  fn cycle_u32(&mut self, field: KnobField, presets: &[u32], forward: bool) {
    let current = self.user_value_u32(field);
    let next = cycle_through(current, presets, forward);
    self.set_user_u32(field, next);
  }

  fn cycle_f32(&mut self, field: KnobField, presets: &[f32], forward: bool) {
    let current = self.user_value_f32(field);
    let next = cycle_through(current, presets, forward);
    self.set_user_f32(field, next);
  }

  fn cycle_enum(&mut self, field: KnobField, forward: bool) {
    // Find the current user-set value inside the `&'static [&'static str]`
    // catalog so cycle_through's `T = &'static str` lifetime detaches
    // from `&self` — that lets us call `set_user_str(&mut self, ...)`
    // immediately after without dragging a borrow. Avoids the prior
    // `Vec<String>` + `Vec<&str>` allocation pair on every keypress.
    let current: Option<&'static str> = self
      .user_value_str(field)
      .and_then(|s| KV_CACHE_TYPES.iter().copied().find(|t| *t == s));
    let next = cycle_through(current, KV_CACHE_TYPES, forward);
    self.set_user_str(field, next.map(|s| s.to_string()));
  }

  fn cycle_bool(&mut self, field: KnobField, forward: bool) {
    // Tri-state: default ↔ on ↔ off (wrap).
    let current = self.user_value_bool(field);
    let next = if forward {
      match current {
        None => Some(true),
        Some(true) => Some(false),
        Some(false) => None,
      }
    } else {
      match current {
        None => Some(false),
        Some(false) => Some(true),
        Some(true) => None,
      }
    };
    self.set_user_bool(field, next);
  }

  /// Backspace on a focused row: clear the user override and re-
  /// inherit from the resolver chain.
  pub fn reset_focused_row(&mut self) {
    match self.field {
      PickerField::Knob(k) => self.clear_user(k),
      PickerField::Extras => {
        self.extras.clear();
      }
    }
  }

  /// Move cursor to the next row.
  pub fn next_field(&mut self) {
    let all = PickerField::all();
    if let Some(i) = all.iter().position(|f| *f == self.field) {
      self.field = all[(i + 1) % all.len()];
    }
  }

  pub fn prev_field(&mut self) {
    let all = PickerField::all();
    if let Some(i) = all.iter().position(|f| *f == self.field) {
      let n = all.len();
      self.field = all[(i + n - 1) % n];
    }
  }

  /// True when the focused row is cyclable (Up/Down would change
  /// the value). `Extras` is non-cyclable; the rest are.
  pub fn focused_field_is_cyclable(&self) -> bool {
    !matches!(self.field, PickerField::Extras)
  }

  /// Read the value the editor row should display, taking the user
  /// override first and the resolver-chain value otherwise.
  pub fn effective_u32(&self, field: KnobField) -> Option<u32> {
    self.user_value_u32(field).or(self.resolved_u32(field))
  }

  pub fn effective_f32(&self, field: KnobField) -> Option<f32> {
    self.user_value_f32(field).or(self.resolved_f32(field))
  }

  pub fn effective_str(&self, field: KnobField) -> Option<String> {
    self
      .user_value_str(field)
      .map(str::to_string)
      .or_else(|| self.resolved_str(field).map(str::to_string))
  }

  pub fn effective_bool(&self, field: KnobField) -> Option<bool> {
    self.user_value_bool(field).or(self.resolved_bool(field))
  }

  /// Source label for `field`. Returns `LayerLabel::User` when the
  /// user has an explicit override; falls back to the resolver's
  /// source map otherwise, then to the spec's `fallback_label` when
  /// the resolver hasn't populated the map yet (freshly-opened
  /// editor before the first resolve).
  pub fn source_for(&self, field: KnobField) -> LayerLabel {
    if self.user_has(field) {
      LayerLabel::User
    } else {
      self
        .sources
        .get(&field)
        .copied()
        .unwrap_or_else(|| crate::launch::flag_aliases::spec_for(field).fallback_label)
    }
  }

  fn user_has(&self, field: KnobField) -> bool {
    match field {
      KnobField::Ctx => self.user_knobs.ctx.is_some(),
      KnobField::Reasoning => self.user_knobs.reasoning.is_some(),
      KnobField::NGpuLayers => self.user_knobs.n_gpu_layers.is_some(),
      KnobField::Threads => self.user_knobs.threads.is_some(),
      KnobField::CacheTypeK => self.user_knobs.cache_type_k.is_some(),
      KnobField::CacheTypeV => self.user_knobs.cache_type_v.is_some(),
      KnobField::FlashAttn => self.user_knobs.flash_attn.is_some(),
      KnobField::Mlock => self.user_knobs.mlock.is_some(),
      KnobField::NoMmap => self.user_knobs.no_mmap.is_some(),
      KnobField::Parallel => self.user_knobs.parallel.is_some(),
      KnobField::BatchSize => self.user_knobs.batch_size.is_some(),
      KnobField::UbatchSize => self.user_knobs.ubatch_size.is_some(),
      KnobField::RopeFreqScale => self.user_knobs.rope_freq_scale.is_some(),
      KnobField::Keep => self.user_knobs.keep.is_some(),
    }
  }

  fn user_value_u32(&self, field: KnobField) -> Option<u32> {
    match field {
      KnobField::Ctx => self.user_knobs.ctx,
      KnobField::NGpuLayers => self.user_knobs.n_gpu_layers,
      KnobField::Threads => self.user_knobs.threads,
      KnobField::Parallel => self.user_knobs.parallel,
      KnobField::BatchSize => self.user_knobs.batch_size,
      KnobField::UbatchSize => self.user_knobs.ubatch_size,
      KnobField::Keep => self.user_knobs.keep,
      _ => None,
    }
  }

  fn user_value_f32(&self, field: KnobField) -> Option<f32> {
    match field {
      KnobField::RopeFreqScale => self.user_knobs.rope_freq_scale,
      _ => None,
    }
  }

  fn user_value_str(&self, field: KnobField) -> Option<&str> {
    match field {
      KnobField::CacheTypeK => self.user_knobs.cache_type_k.as_deref(),
      KnobField::CacheTypeV => self.user_knobs.cache_type_v.as_deref(),
      _ => None,
    }
  }

  fn user_value_bool(&self, field: KnobField) -> Option<bool> {
    match field {
      KnobField::Reasoning => self.user_knobs.reasoning,
      KnobField::FlashAttn => self.user_knobs.flash_attn,
      KnobField::Mlock => self.user_knobs.mlock,
      KnobField::NoMmap => self.user_knobs.no_mmap,
      _ => None,
    }
  }

  fn resolved_u32(&self, field: KnobField) -> Option<u32> {
    match field {
      KnobField::Ctx => self.resolved.ctx,
      KnobField::NGpuLayers => self.resolved.n_gpu_layers,
      KnobField::Threads => self.resolved.threads,
      KnobField::Parallel => self.resolved.parallel,
      KnobField::BatchSize => self.resolved.batch_size,
      KnobField::UbatchSize => self.resolved.ubatch_size,
      KnobField::Keep => self.resolved.keep,
      _ => None,
    }
  }

  fn resolved_f32(&self, field: KnobField) -> Option<f32> {
    match field {
      KnobField::RopeFreqScale => self.resolved.rope_freq_scale,
      _ => None,
    }
  }

  fn resolved_str(&self, field: KnobField) -> Option<&str> {
    match field {
      KnobField::CacheTypeK => self.resolved.cache_type_k.as_deref(),
      KnobField::CacheTypeV => self.resolved.cache_type_v.as_deref(),
      _ => None,
    }
  }

  fn resolved_bool(&self, field: KnobField) -> Option<bool> {
    match field {
      KnobField::Reasoning => self.resolved.reasoning,
      KnobField::FlashAttn => self.resolved.flash_attn,
      KnobField::Mlock => self.resolved.mlock,
      KnobField::NoMmap => self.resolved.no_mmap,
      _ => None,
    }
  }

  pub fn set_user_u32(&mut self, field: KnobField, value: Option<u32>) {
    match field {
      KnobField::Ctx => self.user_knobs.ctx = value,
      KnobField::NGpuLayers => self.user_knobs.n_gpu_layers = value,
      KnobField::Threads => self.user_knobs.threads = value,
      KnobField::Parallel => self.user_knobs.parallel = value,
      KnobField::BatchSize => self.user_knobs.batch_size = value,
      KnobField::UbatchSize => self.user_knobs.ubatch_size = value,
      KnobField::Keep => self.user_knobs.keep = value,
      _ => {}
    }
  }

  pub fn set_user_f32(&mut self, field: KnobField, value: Option<f32>) {
    if matches!(field, KnobField::RopeFreqScale) {
      self.user_knobs.rope_freq_scale = value;
    }
  }

  pub fn set_user_str(&mut self, field: KnobField, value: Option<String>) {
    match field {
      KnobField::CacheTypeK => self.user_knobs.cache_type_k = value,
      KnobField::CacheTypeV => self.user_knobs.cache_type_v = value,
      _ => {}
    }
  }

  pub fn set_user_bool(&mut self, field: KnobField, value: Option<bool>) {
    match field {
      KnobField::Reasoning => self.user_knobs.reasoning = value,
      KnobField::FlashAttn => self.user_knobs.flash_attn = value,
      KnobField::Mlock => self.user_knobs.mlock = value,
      KnobField::NoMmap => self.user_knobs.no_mmap = value,
      _ => {}
    }
  }

  fn clear_user(&mut self, field: KnobField) {
    self.set_user_u32(field, None);
    self.set_user_f32(field, None);
    self.set_user_str(field, None);
    self.set_user_bool(field, None);
  }
}

/// Cycle through `presets` from `current`. Behaviour by case:
///
/// - **`current == None`** (row is on the inherited default): wrap to
///   the first preset (forward) or the last preset (backward).
/// - **`current` matches a preset exactly**: advance / reverse one
///   slot. Falling off either end wraps back to `None` so the row
///   re-inherits.
/// - **`current` sits between presets** (e.g. user typed a custom
///   value via `e`): snap to the nearest preset *in the chosen
///   direction* — pressing `→` jumps to the smallest preset strictly
///   greater than `current`; pressing `←` jumps to the largest one
///   strictly less. This keeps cycling consistent with the visible
///   direction of travel; the previous behaviour of jumping to
///   `presets[0]` was a footgun on custom values mid-list.
///
/// `presets` is assumed to be sorted in ascending order — every
/// caller in [`LaunchPickerState::cycle_knob`] passes a hand-curated
/// ascending list.
fn cycle_through<T: PartialEq + PartialOrd + Copy>(
  current: Option<T>,
  presets: &[T],
  forward: bool,
) -> Option<T> {
  if presets.is_empty() {
    return None;
  }
  match current {
    None => Some(if forward {
      presets[0]
    } else {
      presets[presets.len() - 1]
    }),
    Some(v) => {
      if let Some(i) = presets.iter().position(|p| *p == v) {
        return if forward {
          if i + 1 >= presets.len() {
            None
          } else {
            Some(presets[i + 1])
          }
        } else if i == 0 {
          None
        } else {
          Some(presets[i - 1])
        };
      }
      // Off-preset custom value: snap to the nearest preset in the
      // direction the user pressed. Falls back to first/last when
      // every preset sits on the other side of `current` (e.g. user
      // typed something smaller than `presets[0]` then pressed ←).
      if forward {
        presets
          .iter()
          .find(|p| **p > v)
          .copied()
          .or(Some(presets[presets.len() - 1]))
      } else {
        presets
          .iter()
          .rev()
          .find(|p| **p < v)
          .copied()
          .or(Some(presets[0]))
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn cycle_ctx_walks_through_presets_then_returns_to_native() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.field = PickerField::Knob(KnobField::Ctx);
    assert_eq!(s.user_knobs.ctx, None);
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.ctx, Some(CTX_PRESETS[0]));
    for preset in CTX_PRESETS.iter().skip(1) {
      s.cycle_focused_value_next();
      assert_eq!(s.user_knobs.ctx, Some(*preset));
    }
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.ctx, None, "wraps back to native");
  }

  #[test]
  fn reasoning_cycle_walks_tri_state_in_both_directions() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.field = PickerField::Knob(KnobField::Reasoning);
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.reasoning, Some(true));
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.reasoning, Some(false));
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.reasoning, None);
  }

  #[test]
  fn next_field_iterates_every_picker_row() {
    let mut s = LaunchPickerState::for_model("qwen");
    let all = PickerField::all();
    assert!(
      all.len() > 14,
      "should cover ctx + reasoning + 12 knobs + extras"
    );
    for expected in all.iter().skip(1).chain(std::iter::once(&all[0])) {
      s.next_field();
      assert_eq!(s.field, *expected);
    }
  }

  #[test]
  fn cycle_knob_n_gpu_layers_walks_presets() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.field = PickerField::Knob(KnobField::NGpuLayers);
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.n_gpu_layers, Some(0));
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.n_gpu_layers, Some(16));
  }

  #[test]
  fn cycle_knob_flash_attn_walks_tristate() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.field = PickerField::Knob(KnobField::FlashAttn);
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.flash_attn, Some(true));
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.flash_attn, Some(false));
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.flash_attn, None);
  }

  #[test]
  fn reset_focused_row_clears_user_override() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.field = PickerField::Knob(KnobField::Threads);
    s.cycle_focused_value_next();
    assert!(s.user_knobs.threads.is_some());
    s.reset_focused_row();
    assert!(s.user_knobs.threads.is_none());
  }

  #[test]
  fn source_for_falls_through_to_resolver_when_no_user_override() {
    let mut s = LaunchPickerState::for_model("qwen");
    let mut sources = BTreeMap::new();
    sources.insert(KnobField::NGpuLayers, LayerLabel::ArchDefault);
    s.set_resolved(
      TypedKnobs {
        n_gpu_layers: Some(99),
        ..TypedKnobs::default()
      },
      sources,
    );
    assert_eq!(s.source_for(KnobField::NGpuLayers), LayerLabel::ArchDefault);
    // User override flips the source to User.
    s.user_knobs.n_gpu_layers = Some(32);
    assert_eq!(s.source_for(KnobField::NGpuLayers), LayerLabel::User);
  }

  #[test]
  fn cycle_through_starts_at_first_preset_when_current_is_none() {
    assert_eq!(cycle_through::<u32>(None, &[1, 2, 3], true), Some(1));
    assert_eq!(cycle_through::<u32>(None, &[1, 2, 3], false), Some(3));
  }

  #[test]
  fn cycle_through_wraps_to_none_at_the_end() {
    assert_eq!(cycle_through::<u32>(Some(3), &[1, 2, 3], true), None);
    assert_eq!(cycle_through::<u32>(Some(1), &[1, 2, 3], false), None);
  }

  #[test]
  fn cycle_through_off_preset_snaps_to_nearest_in_direction() {
    // User typed `n_gpu_layers=42` via `e`, then presses →.
    let presets = &[0, 16, 32, 64, 99];
    assert_eq!(cycle_through(Some(42_u32), presets, true), Some(64));
    assert_eq!(cycle_through(Some(42_u32), presets, false), Some(32));
  }

  #[test]
  fn cycle_through_off_preset_below_first_snaps_to_first_going_forward() {
    let presets = &[10, 20, 30];
    // Forward from a value below presets[0] → first preset > current = 10.
    assert_eq!(cycle_through(Some(5_u32), presets, true), Some(10));
    // Backward from below presets[0] has nothing smaller → fall back
    // to first preset.
    assert_eq!(cycle_through(Some(5_u32), presets, false), Some(10));
  }

  #[test]
  fn cycle_through_off_preset_above_last_snaps_to_last_going_backward() {
    let presets = &[10, 20, 30];
    assert_eq!(cycle_through(Some(99_u32), presets, false), Some(30));
    // Forward from above presets[last] has nothing greater → fall back
    // to last preset.
    assert_eq!(cycle_through(Some(99_u32), presets, true), Some(30));
  }
}
