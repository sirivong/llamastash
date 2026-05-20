//! Launch picker form state — the typed-knob editor.
//!
//! The Settings tab renders a vertical list of rows: `ctx`,
//! `reasoning`, every `TypedKnobs` field with a source label
//! (`(user)`, `(last used)`, `(arch default)`, `(built-in)`,
//! `(model default)`), and an `extras` free-text row at the bottom.
//! Up/Down moves between rows; Left/Right cycles the focused row's
//! value; `e` enters inline edit; Enter launches (or commits an
//! open edit); Backspace resets the focused row.

use std::collections::BTreeMap;

use crate::config::TypedKnobs;
use crate::launch::flag_aliases::{KnobField, KV_CACHE_TYPES};
use crate::launch::params::LayerLabel;

/// Pre-canned context-length presets surfaced as quick picks. Custom
/// values flow through the same field when the user types digits.
pub const CTX_PRESETS: &[u32] = &[2048, 4096, 8192, 16384, 32768, 65536, 131072];

/// Tri-state reasoning selector. `ModelDefault` means "don't send
/// the reasoning flag at all" — the daemon falls back to whatever
/// the model's metadata implies. `On` / `Off` are explicit user
/// choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReasoningSetting {
  #[default]
  ModelDefault,
  On,
  Off,
}

impl ReasoningSetting {
  pub fn label(self) -> &'static str {
    match self {
      ReasoningSetting::ModelDefault => "model default",
      ReasoningSetting::On => "on",
      ReasoningSetting::Off => "off",
    }
  }

  pub fn as_wire(self) -> Option<bool> {
    match self {
      ReasoningSetting::ModelDefault => None,
      ReasoningSetting::On => Some(true),
      ReasoningSetting::Off => Some(false),
    }
  }

  pub fn next(self) -> Self {
    match self {
      ReasoningSetting::ModelDefault => ReasoningSetting::On,
      ReasoningSetting::On => ReasoningSetting::Off,
      ReasoningSetting::Off => ReasoningSetting::ModelDefault,
    }
  }

  pub fn prev(self) -> Self {
    match self {
      ReasoningSetting::ModelDefault => ReasoningSetting::Off,
      ReasoningSetting::On => ReasoningSetting::ModelDefault,
      ReasoningSetting::Off => ReasoningSetting::On,
    }
  }

  pub fn from_persisted(prev: bool) -> Self {
    if prev {
      ReasoningSetting::On
    } else {
      ReasoningSetting::Off
    }
  }
}

/// Which row the cursor is on. The editor renders top-to-bottom in
/// this declaration order so [`PickerField::all`] doubles as the
/// vertical-navigation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerField {
  Ctx,
  Reasoning,
  Knob(KnobField),
  Extras,
}

impl PickerField {
  /// All rows in render / navigation order.
  pub fn all() -> Vec<PickerField> {
    let mut v = vec![PickerField::Ctx, PickerField::Reasoning];
    for spec in crate::launch::flag_aliases::knob_specs() {
      v.push(PickerField::Knob(spec.field));
    }
    v.push(PickerField::Extras);
    v
  }
}

/// Inline edit state — set while the user is typing a value for the
/// focused row. `commit` validates the buffer; on success the value
/// lands on the appropriate `TypedKnobs` field and the edit closes.
#[derive(Debug, Clone, Default)]
pub struct InlineEdit {
  pub field: Option<PickerField>,
  pub buffer: String,
  pub cursor: usize,
  /// Inline error rendered under the row when commit fails.
  pub error: Option<String>,
}

impl InlineEdit {
  pub fn open(&mut self, field: PickerField, initial: String) {
    self.field = Some(field);
    self.cursor = initial.len();
    self.buffer = initial;
    self.error = None;
  }

  pub fn close(&mut self) {
    self.field = None;
    self.buffer.clear();
    self.cursor = 0;
    self.error = None;
  }

  pub fn is_open(&self) -> bool {
    self.field.is_some()
  }

  pub fn insert(&mut self, ch: char) {
    self.buffer.insert(self.cursor, ch);
    self.cursor += ch.len_utf8();
    self.error = None;
  }

  pub fn backspace(&mut self) {
    if self.cursor == 0 {
      return;
    }
    let mut new_cursor = self.cursor - 1;
    while !self.buffer.is_char_boundary(new_cursor) {
      new_cursor -= 1;
    }
    self.buffer.replace_range(new_cursor..self.cursor, "");
    self.cursor = new_cursor;
    self.error = None;
  }
}

/// State of the launch picker.
#[derive(Debug, Clone)]
pub struct LaunchPickerState {
  /// Display name of the focused model (rendered in the title).
  pub model_name: String,
  /// Selected ctx length. `None` lets the supervisor honour the
  /// GGUF's native `context_length` (no `-c` flag).
  pub ctx: Option<u32>,
  /// Reasoning bundle: model-default / on / off.
  pub reasoning: ReasoningSetting,
  /// Index into CTX_PRESETS for cycling. `None` means custom.
  pub preset_idx: Option<usize>,
  /// User-supplied typed knobs (only fields the user explicitly set;
  /// every other field stays `None` and inherits from the resolved
  /// chain on render).
  pub user_knobs: TypedKnobs,
  /// Resolved knobs after applying the layered resolver — what the
  /// editor shows for each row.
  pub resolved: TypedKnobs,
  /// Per-knob source labels for the right-aligned origin chip.
  pub sources: BTreeMap<KnobField, LayerLabel>,
  /// Free-form argv tail forwarded to llama-server.
  pub extras: Vec<std::ffi::OsString>,
  /// Edit buffer for the extras row when the user opens it via `e`.
  pub extras_buffer: String,
  pub extras_cursor: usize,
  pub extras_editing: bool,
  /// Inline edit state for numeric / enum rows.
  pub inline_edit: InlineEdit,
  pub field: PickerField,
  pub active_instances: usize,
  pub prefer_port: Option<u16>,
}

impl LaunchPickerState {
  pub fn for_model(model_name: impl Into<String>) -> Self {
    Self {
      model_name: model_name.into(),
      ctx: None,
      reasoning: ReasoningSetting::default(),
      preset_idx: None,
      user_knobs: TypedKnobs::default(),
      resolved: TypedKnobs::default(),
      sources: BTreeMap::new(),
      extras: Vec::new(),
      extras_buffer: String::new(),
      extras_cursor: 0,
      extras_editing: false,
      inline_edit: InlineEdit::default(),
      field: PickerField::Ctx,
      active_instances: 0,
      prefer_port: None,
    }
  }

  /// Seed the resolved knobs + source map from the layered resolver
  /// output. The user-knobs layer is empty on a freshly-opened
  /// editor — the rows show inherited values.
  pub fn set_resolved(&mut self, resolved: TypedKnobs, sources: BTreeMap<KnobField, LayerLabel>) {
    self.resolved = resolved;
    self.sources = sources;
  }

  /// Cycle to the next ctx preset, wrapping around.
  pub fn cycle_ctx_preset(&mut self) {
    let next = match self.preset_idx {
      Some(i) if i + 1 < CTX_PRESETS.len() => Some(i + 1),
      Some(_) => None,
      None => Some(0),
    };
    self.preset_idx = next;
    self.ctx = next.map(|i| CTX_PRESETS[i]);
  }

  pub fn cycle_ctx_preset_prev(&mut self) {
    let next = match self.preset_idx {
      Some(0) => None,
      Some(i) => Some(i - 1),
      None => Some(CTX_PRESETS.len() - 1),
    };
    self.preset_idx = next;
    self.ctx = next.map(|i| CTX_PRESETS[i]);
  }

  pub fn cycle_reasoning_next(&mut self) {
    self.reasoning = self.reasoning.next();
  }

  pub fn cycle_reasoning_prev(&mut self) {
    self.reasoning = self.reasoning.prev();
  }

  /// Cycle the focused field's value forward (Right arrow).
  pub fn cycle_focused_value_next(&mut self) {
    match self.field {
      PickerField::Ctx => self.cycle_ctx_preset(),
      PickerField::Reasoning => self.cycle_reasoning_next(),
      PickerField::Knob(k) => self.cycle_knob(k, true),
      PickerField::Extras => {}
    }
  }

  /// Cycle the focused field's value backward (Left arrow).
  pub fn cycle_focused_value_prev(&mut self) {
    match self.field {
      PickerField::Ctx => self.cycle_ctx_preset_prev(),
      PickerField::Reasoning => self.cycle_reasoning_prev(),
      PickerField::Knob(k) => self.cycle_knob(k, false),
      PickerField::Extras => {}
    }
  }

  fn cycle_knob(&mut self, field: KnobField, forward: bool) {
    match field {
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
    let current = self.user_value_str(field).map(str::to_string);
    let presets: Vec<String> = KV_CACHE_TYPES.iter().map(|s| s.to_string()).collect();
    let presets_ref: Vec<&str> = presets.iter().map(String::as_str).collect();
    let next = cycle_through(current.as_deref(), &presets_ref, forward).map(str::to_string);
    self.set_user_str(field, next);
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
      PickerField::Ctx => {
        self.ctx = None;
        self.preset_idx = None;
      }
      PickerField::Reasoning => {
        self.reasoning = ReasoningSetting::ModelDefault;
      }
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
  /// source map otherwise.
  pub fn source_for(&self, field: KnobField) -> LayerLabel {
    if self.user_has(field) {
      LayerLabel::User
    } else {
      self
        .sources
        .get(&field)
        .copied()
        .unwrap_or(LayerLabel::ModelDefault)
    }
  }

  fn user_has(&self, field: KnobField) -> bool {
    match field {
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
      KnobField::FlashAttn => self.user_knobs.flash_attn,
      KnobField::Mlock => self.user_knobs.mlock,
      KnobField::NoMmap => self.user_knobs.no_mmap,
      _ => None,
    }
  }

  fn resolved_u32(&self, field: KnobField) -> Option<u32> {
    match field {
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
      KnobField::FlashAttn => self.resolved.flash_attn,
      KnobField::Mlock => self.resolved.mlock,
      KnobField::NoMmap => self.resolved.no_mmap,
      _ => None,
    }
  }

  pub fn set_user_u32(&mut self, field: KnobField, value: Option<u32>) {
    match field {
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

/// Cycle through `presets` from `current`. When `current` is `None`,
/// wrap at the end; when it matches a preset, advance / reverse;
/// when it sits between presets, snap to the nearest in the chosen
/// direction.
fn cycle_through<T: PartialEq + Copy>(
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
      let idx = presets.iter().position(|p| *p == v);
      match idx {
        Some(i) => {
          if forward {
            if i + 1 >= presets.len() {
              None
            } else {
              Some(presets[i + 1])
            }
          } else if i == 0 {
            None
          } else {
            Some(presets[i - 1])
          }
        }
        None => Some(if forward {
          presets[0]
        } else {
          presets[presets.len() - 1]
        }),
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
    assert_eq!(s.ctx, None);
    s.cycle_ctx_preset();
    assert_eq!(s.ctx, Some(CTX_PRESETS[0]));
    for preset in CTX_PRESETS.iter().skip(1) {
      s.cycle_ctx_preset();
      assert_eq!(s.ctx, Some(*preset));
    }
    s.cycle_ctx_preset();
    assert_eq!(s.ctx, None, "wraps back to native");
  }

  #[test]
  fn reasoning_cycle_walks_tri_state_in_both_directions() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.cycle_reasoning_next();
    assert_eq!(s.reasoning, ReasoningSetting::On);
    s.cycle_reasoning_next();
    assert_eq!(s.reasoning, ReasoningSetting::Off);
    s.cycle_reasoning_next();
    assert_eq!(s.reasoning, ReasoningSetting::ModelDefault);
  }

  #[test]
  fn next_field_iterates_every_picker_row() {
    let mut s = LaunchPickerState::for_model("qwen");
    let all = PickerField::all();
    assert!(
      all.len() > 12,
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
    sources.insert(KnobField::NGpuLayers, LayerLabel::BuiltIn);
    s.set_resolved(
      TypedKnobs {
        n_gpu_layers: Some(99),
        ..TypedKnobs::default()
      },
      sources,
    );
    assert_eq!(s.source_for(KnobField::NGpuLayers), LayerLabel::BuiltIn);
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
}
