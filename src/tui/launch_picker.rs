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

use crate::config::{KnobValue, KnobValueOpt, TypedKnobs};
use crate::launch::flag_aliases::{
  knob_display_groups, knob_row_visible, KnobField, KV_CACHE_TYPES, SPLIT_MODES,
};
use crate::launch::params::{BackendChoice, LayerLabel};

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

/// Lazily-built navigation order — every knob in editor **display**
/// order (the flattened [`knob_display_groups`], which clusters knobs
/// by function and is distinct from the pinned argv order), followed
/// by `Extras`. Built once on first access so per-keypress navigation
/// does no allocation.
static ALL_FIELDS: LazyLock<Box<[PickerField]>> = LazyLock::new(|| {
  let mut v: Vec<PickerField> = Vec::new();
  for group in knob_display_groups() {
    for field in group.fields {
      v.push(PickerField::Knob(*field));
    }
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
  ///   open an [`crate::tui::input_field::InputField`] for typing.
  /// - Boolean knobs (reasoning, flash_attn, mlock, no_mmap) don't —
  ///   they're cycled with ←/→. Surfacing `e:edit` on a boolean row
  ///   would be a no-op chip and a misleading affordance.
  ///
  /// Shared between the Settings-row edit handler (which early-returns
  /// on booleans) and the right-pane hint strip (which hides the chip
  /// on those rows) so the chip and the handler stay in lockstep.
  pub fn is_editable(self) -> bool {
    match self {
      PickerField::Extras => true,
      PickerField::Knob(k) => match k {
        KnobField::Reasoning | KnobField::FlashAttn | KnobField::Mlock | KnobField::NoMmap => false,
        KnobField::Ctx
        | KnobField::NGpuLayers
        | KnobField::NCpuMoe
        | KnobField::Threads
        | KnobField::Parallel
        | KnobField::BatchSize
        | KnobField::UbatchSize
        | KnobField::Keep
        | KnobField::RopeFreqScale
        | KnobField::CacheTypeK
        | KnobField::CacheTypeV
        | KnobField::Device
        | KnobField::TensorSplit
        | KnobField::MainGpu
        | KnobField::SplitMode => true,
      },
    }
  }
}

/// Inline-edit state owned by [`LaunchPickerState`].
///
/// The buffer and modal `editing` flag live in `inline_edit`
/// ([`crate::tui::input_field::InputField`]) so the typed-knob editor shares the
/// `e:edit / Esc:walk-back / Enter:Submit` contract with every
/// other text input in the TUI. The wrapper carries the two extra
/// pieces of state `crate::tui::input_field::InputField` doesn't model:
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
  /// edit is open *and* `crate::tui::input_field::InputField` reports
  /// edit mode). Used by
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
  /// [`crate::tui::input_field::InputField`] plus the `PickerField`
  /// marker so the commit path
  /// knows which row to write back to, and an optional parse-error
  /// string rendered under the row.
  pub inline_edit: InlineEdit,
  pub field: PickerField,
  /// The focused model's *own* concrete backend (the one its catalog source
  /// binds to): `LlamaCpp` for a GGUF, `Lemonade` for a `lemonade://`
  /// registry model. A model lives in exactly one backend's catalog, so
  /// there is no user-facing chooser — this drives knob visibility (R6)
  /// and is what the launch dispatches to. Never `Auto`.
  pub model_backend: BackendChoice,
  pub active_instances: usize,
  pub prefer_port: Option<u16>,
  /// Launch device catalog from `status.device_catalog` — the exact
  /// `--device` selectors the daemon's configured binaries will accept,
  /// each tagged with its owning binary. The Device row cycles through
  /// this flat list (one entry per backend-view of a card, e.g. the
  /// same physical card may appear as both `ROCm0` and `Vulkan0`).
  /// `user_knobs.device` stores the chosen selector verbatim.
  pub device_catalog: Vec<crate::launch::list_devices::LaunchDevice>,
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
      model_backend: BackendChoice::LlamaCpp,
      active_instances: 0,
      prefer_port: None,
      device_catalog: Vec::new(),
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

  /// Whether the model's backend honors `field` (R6).
  /// [`Self::field_visible`] hides rows the backend can't honor.
  /// llama.cpp honors every typed knob; Lemonade honors `ctx` only
  /// (lemond's `ctx_size` load option).
  pub fn knob_supported(&self, field: KnobField) -> bool {
    use crate::backend::lemonade::LemonadeBackend;
    use crate::backend::llama_cpp::LlamaCppBackend;
    use crate::backend::Backend;
    match self.model_backend {
      BackendChoice::Lemonade => LemonadeBackend::new().capabilities().supports(field),
      _ => LlamaCppBackend::new().capabilities().supports(field),
    }
  }

  /// The model's backend id for log / label use.
  pub fn active_backend_id(&self) -> &'static str {
    match self.model_backend {
      BackendChoice::Lemonade => "lemonade",
      _ => "llamacpp",
    }
  }

  fn cycle_knob(&mut self, field: KnobField, forward: bool) {
    match field {
      KnobField::Ctx => self.cycle_u32(field, CTX_PRESETS, forward),
      KnobField::Reasoning => self.cycle_bool(field, forward),
      KnobField::NGpuLayers => self.cycle_u32(field, &[0, 16, 32, 64, 99], forward),
      KnobField::NCpuMoe => self.cycle_u32(field, &[0, 4, 8, 16, 32, 64], forward),
      KnobField::Threads => self.cycle_u32(field, &[1, 2, 4, 6, 8, 12, 16, 24], forward),
      KnobField::Parallel => self.cycle_u32(field, &[1, 2, 4, 8, 16], forward),
      KnobField::BatchSize => self.cycle_u32(field, &[256, 512, 1024, 2048, 4096], forward),
      KnobField::UbatchSize => self.cycle_u32(field, &[128, 256, 512, 1024], forward),
      KnobField::Keep => self.cycle_u32(field, &[0, 64, 128, 256, 512, 1024], forward),
      KnobField::MainGpu => self.cycle_u32(field, &[0, 1, 2, 3], forward),
      KnobField::RopeFreqScale => self.cycle_f32(field, &[0.5, 1.0, 2.0, 4.0], forward),
      KnobField::CacheTypeK | KnobField::CacheTypeV => {
        self.cycle_str_set(field, KV_CACHE_TYPES, forward)
      }
      KnobField::SplitMode => self.cycle_str_set(field, SPLIT_MODES, forward),
      KnobField::FlashAttn | KnobField::Mlock | KnobField::NoMmap => {
        self.cycle_bool(field, forward)
      }
      KnobField::Device => self.cycle_device(field, forward),
      // Free-form ratio with no natural preset set — edited via `e`.
      // ←/→ is a deliberate no-op (there's nothing to cycle through).
      KnobField::TensorSplit => {}
    }
  }

  /// True when the user explicitly cycled this knob to `Auto`.
  fn user_is_auto(&self, field: KnobField) -> bool {
    crate::launch::field_is_auto(&self.user_knobs, field)
  }

  /// Set the user override for `field` to `Auto` (delegate to `--fit`).
  fn set_user_auto(&mut self, field: KnobField) {
    crate::launch::set_field_auto(&mut self.user_knobs, field);
  }

  fn cycle_u32(&mut self, field: KnobField, presets: &[u32], forward: bool) {
    let allow_auto = field.fit_governed();
    let cur = if allow_auto && self.user_is_auto(field) {
      CycleState::Auto
    } else {
      match self.user_value_u32(field) {
        Some(v) => CycleState::Set(v),
        None => CycleState::Inherited,
      }
    };
    match ring_next(cur, presets, forward, allow_auto) {
      CycleState::Inherited => self.set_user_u32(field, None),
      CycleState::Auto => self.set_user_auto(field),
      CycleState::Set(v) => self.set_user_u32(field, Some(v)),
    }
  }

  fn cycle_f32(&mut self, field: KnobField, presets: &[f32], forward: bool) {
    let allow_auto = field.fit_governed();
    let cur = if allow_auto && self.user_is_auto(field) {
      CycleState::Auto
    } else {
      match self.user_value_f32(field) {
        Some(v) => CycleState::Set(v),
        None => CycleState::Inherited,
      }
    };
    match ring_next(cur, presets, forward, allow_auto) {
      CycleState::Inherited => self.set_user_f32(field, None),
      CycleState::Auto => self.set_user_auto(field),
      CycleState::Set(v) => self.set_user_f32(field, Some(v)),
    }
  }

  /// Cycle a constrained-string knob (`cache_type_*`, `split_mode`,
  /// `tensor_split`). Fit-governed knobs carry the `Auto` stop; the rest
  /// cycle `Inherited → set… → wrap`.
  fn cycle_str_set(&mut self, field: KnobField, set: &'static [&'static str], forward: bool) {
    let allow_auto = field.fit_governed();
    let cur = if allow_auto && self.user_is_auto(field) {
      CycleState::Auto
    } else {
      // Detach the `&'static str` from `&self` so `set_user_str(&mut …)`
      // can run right after without dragging a borrow.
      match self
        .user_value_str(field)
        .and_then(|s| set.iter().copied().find(|t| *t == s))
      {
        Some(v) => CycleState::Set(v),
        None => CycleState::Inherited,
      }
    };
    match ring_next(cur, set, forward, allow_auto) {
      CycleState::Inherited => self.set_user_str(field, None),
      CycleState::Auto => self.set_user_auto(field),
      CycleState::Set(v) => self.set_user_str(field, Some(v.to_string())),
    }
  }

  fn cycle_bool(&mut self, field: KnobField, forward: bool) {
    // Fit-governed bools get the quad ring `Inherited → Auto → on → off`;
    // non-fit bools (every bool knob today — flash_attn, mlock, no_mmap,
    // reasoning) drop the no-op Auto stop to a tri ring
    // `Inherited → on → off`.
    let allow_auto = field.fit_governed();
    let cur = if allow_auto && self.user_is_auto(field) {
      CycleState::Auto
    } else {
      match self.user_value_bool(field) {
        Some(b) => CycleState::Set(b),
        None => CycleState::Inherited,
      }
    };
    let next = if forward {
      match cur {
        CycleState::Inherited if allow_auto => CycleState::Auto,
        CycleState::Inherited => CycleState::Set(true),
        CycleState::Auto => CycleState::Set(true),
        CycleState::Set(true) => CycleState::Set(false),
        CycleState::Set(false) => CycleState::Inherited,
      }
    } else {
      match cur {
        CycleState::Inherited => CycleState::Set(false),
        CycleState::Set(false) => CycleState::Set(true),
        CycleState::Set(true) if allow_auto => CycleState::Auto,
        CycleState::Set(true) => CycleState::Inherited,
        CycleState::Auto => CycleState::Inherited,
      }
    };
    match next {
      CycleState::Inherited => self.set_user_bool(field, None),
      CycleState::Auto => self.set_user_auto(field),
      CycleState::Set(b) => self.set_user_bool(field, Some(b)),
    }
  }

  /// Cycle the Device row through the flat catalog. The cycle space is
  /// `[default] + catalog selectors`: stepping off either end wraps back
  /// to `None`. `default` already means "let llama-server pick the
  /// device(s)", so the row carries no separate `Auto` stop (device is
  /// not fit-governed). `user_knobs.device` holds the chosen selector
  /// verbatim (`"Vulkan1"`), or `None` for default.
  fn cycle_device(&mut self, field: KnobField, forward: bool) {
    if self.device_catalog.is_empty() {
      // No catalog (CPU-only / no binary enumerated) — only the
      // default exists, so any cycle resets to it.
      self.set_user_str(field, None);
      return;
    }
    let selectors: Vec<&str> = self
      .device_catalog
      .iter()
      .map(|d| d.selector.as_str())
      .collect();
    // Cycle positions: 0 = default (None), 1+i = selector[i]. A stale
    // Auto coerces to default (position 0).
    let cur_pos: usize = match self.user_value_str(field).filter(|s| !s.is_empty()) {
      None => 0,
      Some(sel) => selectors
        .iter()
        .position(|s| *s == sel)
        .map(|i| i + 1)
        .unwrap_or(0),
    };
    let len = selectors.len() + 1; // +default
    let next_pos = if forward {
      (cur_pos + 1) % len
    } else {
      (cur_pos + len - 1) % len
    };
    match next_pos {
      0 => self.set_user_str(field, None),
      i => self.set_user_str(field, Some(selectors[i - 1].to_string())),
    }
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

  /// Whether the host exposes more than one selectable device. The
  /// `device` row is hidden (rendered and skipped in navigation) when
  /// `false` so single-GPU / CPU-only users don't see a row that can
  /// only ever hold `default`.
  pub fn multi_device(&self) -> bool {
    self.device_catalog.len() > 1
  }

  /// Whether a row is currently shown / navigable. The Multi-GPU
  /// placement knobs (`device`, `tensor_split`, `main_gpu`,
  /// `split_mode`) are gated on [`Self::multi_device`]; knobs the
  /// model's backend can't honor are hidden outright (R6) — a Lemonade
  /// model shows just ctx and extras rather than a column of dead
  /// rows. Delegates to the single-source group table.
  pub fn field_visible(&self, field: PickerField) -> bool {
    match field {
      PickerField::Knob(k) => self.knob_supported(k) && knob_row_visible(k, self.multi_device()),
      PickerField::Extras => true,
    }
  }

  /// Move cursor to the next visible row.
  pub fn next_field(&mut self) {
    self.step_field(true);
  }

  /// Move cursor to the previous visible row.
  pub fn prev_field(&mut self) {
    self.step_field(false);
  }

  /// Advance the cursor one step in `forward`/back direction, skipping
  /// any hidden rows (e.g. `device` on single-GPU hosts).
  fn step_field(&mut self, forward: bool) {
    let all = PickerField::all();
    let Some(i) = all.iter().position(|f| *f == self.field) else {
      return;
    };
    let n = all.len();
    for step in 1..=n {
      let idx = if forward {
        (i + step) % n
      } else {
        (i + n - step) % n
      };
      if self.field_visible(all[idx]) {
        self.field = all[idx];
        return;
      }
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

  /// Whether the row's *effective* state is `Auto` — either the user
  /// cycled it to Auto, or (untouched) it resolved to Auto via the
  /// seeding rule / a remembered Auto. Drives the `Auto` value label.
  pub fn effective_is_auto(&self, field: KnobField) -> bool {
    if self.user_has(field) {
      self.user_is_auto(field)
    } else {
      crate::launch::field_is_auto(&self.resolved, field)
    }
  }

  fn user_has(&self, field: KnobField) -> bool {
    match field {
      KnobField::Ctx => self.user_knobs.ctx.is_some(),
      KnobField::Reasoning => self.user_knobs.reasoning.is_some(),
      KnobField::NGpuLayers => self.user_knobs.n_gpu_layers.is_some(),
      KnobField::NCpuMoe => self.user_knobs.n_cpu_moe.is_some(),
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
      KnobField::Device => self.user_knobs.device.is_some(),
      KnobField::TensorSplit => self.user_knobs.tensor_split.is_some(),
      KnobField::MainGpu => self.user_knobs.main_gpu.is_some(),
      KnobField::SplitMode => self.user_knobs.split_mode.is_some(),
    }
  }

  fn user_value_u32(&self, field: KnobField) -> Option<u32> {
    match field {
      KnobField::Ctx => self.user_knobs.ctx.set_value().copied(),
      KnobField::NGpuLayers => self.user_knobs.n_gpu_layers.set_value().copied(),
      KnobField::NCpuMoe => self.user_knobs.n_cpu_moe.set_value().copied(),
      KnobField::Threads => self.user_knobs.threads.set_value().copied(),
      KnobField::Parallel => self.user_knobs.parallel.set_value().copied(),
      KnobField::BatchSize => self.user_knobs.batch_size.set_value().copied(),
      KnobField::UbatchSize => self.user_knobs.ubatch_size.set_value().copied(),
      KnobField::Keep => self.user_knobs.keep.set_value().copied(),
      KnobField::MainGpu => self.user_knobs.main_gpu.set_value().copied(),
      _ => None,
    }
  }

  fn user_value_f32(&self, field: KnobField) -> Option<f32> {
    match field {
      KnobField::RopeFreqScale => self.user_knobs.rope_freq_scale.set_value().copied(),
      _ => None,
    }
  }

  fn user_value_str(&self, field: KnobField) -> Option<&str> {
    match field {
      KnobField::CacheTypeK => self.user_knobs.cache_type_k.set_value().map(String::as_str),
      KnobField::CacheTypeV => self.user_knobs.cache_type_v.set_value().map(String::as_str),
      KnobField::Device => self.user_knobs.device.set_value().map(String::as_str),
      KnobField::TensorSplit => self.user_knobs.tensor_split.set_value().map(String::as_str),
      KnobField::SplitMode => self.user_knobs.split_mode.set_value().map(String::as_str),
      _ => None,
    }
  }

  fn user_value_bool(&self, field: KnobField) -> Option<bool> {
    match field {
      KnobField::Reasoning => self.user_knobs.reasoning.set_value().copied(),
      KnobField::FlashAttn => self.user_knobs.flash_attn.set_value().copied(),
      KnobField::Mlock => self.user_knobs.mlock.set_value().copied(),
      KnobField::NoMmap => self.user_knobs.no_mmap.set_value().copied(),
      _ => None,
    }
  }

  fn resolved_u32(&self, field: KnobField) -> Option<u32> {
    match field {
      KnobField::Ctx => self.resolved.ctx.set_value().copied(),
      KnobField::NGpuLayers => self.resolved.n_gpu_layers.set_value().copied(),
      KnobField::NCpuMoe => self.resolved.n_cpu_moe.set_value().copied(),
      KnobField::Threads => self.resolved.threads.set_value().copied(),
      KnobField::Parallel => self.resolved.parallel.set_value().copied(),
      KnobField::BatchSize => self.resolved.batch_size.set_value().copied(),
      KnobField::UbatchSize => self.resolved.ubatch_size.set_value().copied(),
      KnobField::Keep => self.resolved.keep.set_value().copied(),
      KnobField::MainGpu => self.resolved.main_gpu.set_value().copied(),
      _ => None,
    }
  }

  fn resolved_f32(&self, field: KnobField) -> Option<f32> {
    match field {
      KnobField::RopeFreqScale => self.resolved.rope_freq_scale.set_value().copied(),
      _ => None,
    }
  }

  fn resolved_str(&self, field: KnobField) -> Option<&str> {
    match field {
      KnobField::CacheTypeK => self.resolved.cache_type_k.set_value().map(String::as_str),
      KnobField::CacheTypeV => self.resolved.cache_type_v.set_value().map(String::as_str),
      KnobField::Device => self.resolved.device.set_value().map(String::as_str),
      KnobField::TensorSplit => self.resolved.tensor_split.set_value().map(String::as_str),
      KnobField::SplitMode => self.resolved.split_mode.set_value().map(String::as_str),
      _ => None,
    }
  }

  fn resolved_bool(&self, field: KnobField) -> Option<bool> {
    match field {
      KnobField::Reasoning => self.resolved.reasoning.set_value().copied(),
      KnobField::FlashAttn => self.resolved.flash_attn.set_value().copied(),
      KnobField::Mlock => self.resolved.mlock.set_value().copied(),
      KnobField::NoMmap => self.resolved.no_mmap.set_value().copied(),
      _ => None,
    }
  }

  pub fn set_user_u32(&mut self, field: KnobField, value: Option<u32>) {
    // An explicit value sets `Set(v)`; clearing (`None`) drops the slot
    // back to Inherited. The Auto state is set via the cycle helpers,
    // not this typed-value setter.
    let value = value.map(KnobValue::Set);
    match field {
      KnobField::Ctx => self.user_knobs.ctx = value,
      KnobField::NGpuLayers => self.user_knobs.n_gpu_layers = value,
      KnobField::NCpuMoe => self.user_knobs.n_cpu_moe = value,
      KnobField::Threads => self.user_knobs.threads = value,
      KnobField::Parallel => self.user_knobs.parallel = value,
      KnobField::BatchSize => self.user_knobs.batch_size = value,
      KnobField::UbatchSize => self.user_knobs.ubatch_size = value,
      KnobField::Keep => self.user_knobs.keep = value,
      KnobField::MainGpu => self.user_knobs.main_gpu = value,
      _ => {}
    }
  }

  pub fn set_user_f32(&mut self, field: KnobField, value: Option<f32>) {
    if matches!(field, KnobField::RopeFreqScale) {
      self.user_knobs.rope_freq_scale = value.map(KnobValue::Set);
    }
  }

  pub fn set_user_str(&mut self, field: KnobField, value: Option<String>) {
    let value = value.map(KnobValue::Set);
    match field {
      KnobField::CacheTypeK => self.user_knobs.cache_type_k = value,
      KnobField::CacheTypeV => self.user_knobs.cache_type_v = value,
      KnobField::Device => self.user_knobs.device = value,
      KnobField::TensorSplit => self.user_knobs.tensor_split = value,
      KnobField::SplitMode => self.user_knobs.split_mode = value,
      _ => {}
    }
  }

  pub fn set_user_bool(&mut self, field: KnobField, value: Option<bool>) {
    let value = value.map(KnobValue::Set);
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

  /// Display label for the Device row. Resolves the selector against
  /// the catalog to show `"<name> (<backend>)"` (e.g.
  /// `"NVIDIA GeForce RTX 3080 (Vulkan)"`). Falls back to the raw
  /// selector when it isn't in the catalog (stale persisted value).
  /// Returns `"default"` when no device is selected.
  pub fn device_value_display(&self) -> String {
    let sel = self
      .effective_str(KnobField::Device)
      .filter(|v| !v.is_empty());
    sel
      .map(
        |s| match self.device_catalog.iter().find(|d| d.selector == s) {
          Some(d) => format!("{} ({})", d.name, d.backend),
          None => s.to_string(),
        },
      )
      .unwrap_or_else(|| "default".into())
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
/// A knob's position in the Auto-aware cycle ring:
/// `Inherited → Auto → Set(preset)… → wrap`.
enum CycleState<T> {
  /// `None` knob — inherits from the resolver chain.
  Inherited,
  /// Delegated to `--fit`.
  Auto,
  /// Pinned to a concrete value.
  Set(T),
}

/// Step a value knob one slot around the ring `Inherited → Auto →
/// preset[0] → … → preset[last] → Inherited`. Custom (off-preset)
/// values snap to the nearest preset in the travel direction via
/// [`cycle_through`]; falling off the top end lands on `Inherited`,
/// off the bottom on `Auto`.
fn ring_next<T: PartialEq + PartialOrd + Copy>(
  current: CycleState<T>,
  presets: &[T],
  forward: bool,
  allow_auto: bool,
) -> CycleState<T> {
  // Non-fit-governed knobs have no Auto stop: a two-stop ring
  // `Inherited → presets… → Inherited`. A stray Auto (e.g. a stale
  // persisted value) coerces back to Inherited so cycling escapes it.
  if !allow_auto {
    return match current {
      CycleState::Auto => CycleState::Inherited,
      CycleState::Inherited => {
        cycle_through(None, presets, forward).map_or(CycleState::Inherited, CycleState::Set)
      }
      CycleState::Set(v) => {
        cycle_through(Some(v), presets, forward).map_or(CycleState::Inherited, CycleState::Set)
      }
    };
  }
  match current {
    CycleState::Inherited => {
      if forward {
        CycleState::Auto
      } else {
        // Backward from Inherited wraps to the last preset.
        cycle_through(None, presets, false).map_or(CycleState::Auto, CycleState::Set)
      }
    }
    CycleState::Auto => {
      if forward {
        cycle_through(None, presets, true).map_or(CycleState::Inherited, CycleState::Set)
      } else {
        CycleState::Inherited
      }
    }
    CycleState::Set(v) => match cycle_through(Some(v), presets, forward) {
      Some(p) => CycleState::Set(p),
      // Off the top → Inherited; off the bottom → Auto.
      None => {
        if forward {
          CycleState::Inherited
        } else {
          CycleState::Auto
        }
      }
    },
  }
}

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
  #![allow(clippy::useless_conversion)]
  use super::*;

  #[test]
  fn cycle_ctx_walks_through_presets_then_returns_to_native() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.field = PickerField::Knob(KnobField::Ctx);
    assert_eq!(s.user_knobs.ctx, None);
    // Ring: Inherited → Auto → presets… → wrap to Inherited.
    s.cycle_focused_value_next();
    assert_eq!(
      s.user_knobs.ctx,
      Some(KnobValue::Auto),
      "first stop is Auto"
    );
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.ctx, Some(KnobValue::Set(CTX_PRESETS[0])));
    for preset in CTX_PRESETS.iter().skip(1) {
      s.cycle_focused_value_next();
      assert_eq!(s.user_knobs.ctx, Some(KnobValue::Set(*preset)));
    }
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.ctx, None, "wraps back to inherited");
  }

  #[test]
  fn reasoning_cycle_walks_tri_state_in_both_directions() {
    // `reasoning` is not fit-governed, so it has no `Auto` stop: the
    // ring is Inherited → on → off → Inherited.
    let mut s = LaunchPickerState::for_model("qwen");
    s.field = PickerField::Knob(KnobField::Reasoning);
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.reasoning, Some(KnobValue::Set(true)));
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.reasoning, Some(KnobValue::Set(false)));
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.reasoning, None);
    // Backward from Inherited lands on off (the far end of the ring).
    s.cycle_focused_value_prev();
    assert_eq!(s.user_knobs.reasoning, Some(KnobValue::Set(false)));
    s.cycle_focused_value_prev();
    assert_eq!(s.user_knobs.reasoning, Some(KnobValue::Set(true)));
    s.cycle_focused_value_prev();
    assert_eq!(s.user_knobs.reasoning, None);
  }

  #[test]
  fn gguf_model_honors_every_llamacpp_knob() {
    // Default model_backend is LlamaCpp (a GGUF row).
    let s = LaunchPickerState::for_model("qwen");
    assert!(s.knob_supported(KnobField::Ctx));
    assert_eq!(s.active_backend_id(), "llamacpp");
  }

  #[test]
  fn lemonade_model_shows_only_ctx_and_extras() {
    let mut s = LaunchPickerState::for_model("Qwen2.5-7B");
    s.model_backend = BackendChoice::Lemonade;
    // Lemonade honors `ctx` (lemond's `ctx_size` load option) and the
    // free-form extras (`*_args`) — every other llama.cpp knob row is
    // hidden outright. There is no Backend row: a model lives in exactly
    // one backend's catalog, so there is nothing to choose.
    let visible: Vec<PickerField> = PickerField::all()
      .iter()
      .copied()
      .filter(|f| s.field_visible(*f))
      .collect();
    assert_eq!(
      visible,
      vec![PickerField::Knob(KnobField::Ctx), PickerField::Extras],
      "lemonade picker is ctx + extras, nothing else"
    );
    assert_eq!(s.active_backend_id(), "lemonade");
  }

  #[test]
  fn next_field_iterates_every_visible_picker_row() {
    let mut s = LaunchPickerState::for_model("qwen");
    // A 2-device catalog makes the `device` row visible so navigation
    // visits every row. The single-GPU skip is covered separately.
    s.device_catalog = vec![
      dev("CUDA0", "CUDA", "GPU 0", "/usr/bin/llama-server"),
      dev("CUDA1", "CUDA", "GPU 1", "/usr/bin/llama-server"),
    ];
    // Visible rows in nav order. The Backend chooser is hidden with a single
    // concrete backend, so it's skipped (covered by `field_visible`).
    let visible: Vec<PickerField> = PickerField::all()
      .iter()
      .copied()
      .filter(|f| s.field_visible(*f))
      .collect();
    assert!(
      visible.len() > 14,
      "should cover every typed knob (ctx, reasoning, offload, placement, …) + extras"
    );
    let start_idx = visible
      .iter()
      .position(|f| *f == s.field)
      .expect("the initial field is visible");
    for step in 1..=visible.len() {
      s.next_field();
      assert_eq!(s.field, visible[(start_idx + step) % visible.len()]);
    }
  }

  #[test]
  fn navigation_skips_multi_gpu_rows_on_single_gpu() {
    // Default picker has an empty catalog → the whole Multi-GPU
    // placement group is hidden, so neither next_field nor prev_field
    // ever lands on any of its rows.
    let mut s = LaunchPickerState::for_model("qwen");
    assert!(!s.multi_device());
    let hidden = [
      KnobField::Device,
      KnobField::TensorSplit,
      KnobField::MainGpu,
      KnobField::SplitMode,
    ];
    let n = PickerField::all().len();
    for _ in 0..n {
      s.next_field();
      for f in hidden {
        assert_ne!(s.field, PickerField::Knob(f), "landed on hidden {f:?}");
      }
    }
    for _ in 0..n {
      s.prev_field();
      for f in hidden {
        assert_ne!(s.field, PickerField::Knob(f), "landed on hidden {f:?}");
      }
    }
  }

  #[test]
  fn cycle_knob_n_gpu_layers_walks_presets() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.field = PickerField::Knob(KnobField::NGpuLayers);
    s.cycle_focused_value_next();
    assert_eq!(
      s.user_knobs.n_gpu_layers,
      Some(KnobValue::Auto),
      "Auto stop first"
    );
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.n_gpu_layers, Some(KnobValue::Set(0)));
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.n_gpu_layers, Some(KnobValue::Set(16)));
  }

  #[test]
  fn cycle_knob_flash_attn_walks_tristate() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.field = PickerField::Knob(KnobField::FlashAttn);
    // flash_attn is not fit-governed → no Auto stop. Ring:
    // Inherited → on → off → Inherited.
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.flash_attn, Some(KnobValue::Set(true)));
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.flash_attn, Some(KnobValue::Set(false)));
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.flash_attn, None);
  }

  #[test]
  fn effective_is_auto_tracks_user_then_resolved_state() {
    let mut s = LaunchPickerState::for_model("qwen");
    assert!(
      !s.effective_is_auto(KnobField::Ctx),
      "fresh knob is not Auto"
    );
    // A user-cycled Auto reads as Auto.
    s.set_user_auto(KnobField::Ctx);
    assert!(s.effective_is_auto(KnobField::Ctx));
    // An untouched knob reflects the resolved (seeded / remembered) Auto.
    s.set_resolved(
      TypedKnobs {
        n_gpu_layers: Some(KnobValue::Auto),
        ..TypedKnobs::default()
      },
      BTreeMap::new(),
    );
    assert!(
      s.effective_is_auto(KnobField::NGpuLayers),
      "resolved Auto shows when the user hasn't overridden the row"
    );
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
        n_gpu_layers: Some(KnobValue::Set(99)),
        ..TypedKnobs::default()
      },
      sources,
    );
    assert_eq!(s.source_for(KnobField::NGpuLayers), LayerLabel::ArchDefault);
    // User override flips the source to User.
    s.user_knobs.n_gpu_layers = Some(KnobValue::Set(32));
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

  // ---- Flat device-catalog picker tests ----

  use crate::launch::list_devices::LaunchDevice;

  /// Build a `LaunchDevice` for tests. Memory fields don't affect the
  /// picker logic, so they're left `None`.
  fn dev(selector: &str, backend: &str, name: &str, binary: &str) -> LaunchDevice {
    LaunchDevice {
      selector: selector.into(),
      backend: backend.into(),
      name: name.into(),
      binary: std::path::PathBuf::from(binary),
      total_mib: None,
      free_mib: None,
    }
  }

  fn catalog_two_vendors() -> Vec<LaunchDevice> {
    vec![
      dev(
        "Vulkan0",
        "Vulkan",
        "AMD Radeon AI PRO R9700",
        "/vk/llama-server",
      ),
      dev(
        "Vulkan1",
        "Vulkan",
        "NVIDIA GeForce RTX 3080",
        "/vk/llama-server",
      ),
      dev(
        "ROCm0",
        "ROCm",
        "AMD Radeon AI PRO R9700",
        "/rocm/llama-server",
      ),
    ]
  }

  #[test]
  fn device_value_display_resolves_selector_to_name_and_backend() {
    let mut s = LaunchPickerState::for_model("test");
    s.device_catalog = catalog_two_vendors();
    // No selection → default.
    assert_eq!(s.device_value_display(), "default");
    s.set_user_str(KnobField::Device, Some("Vulkan1".into()));
    assert_eq!(s.device_value_display(), "NVIDIA GeForce RTX 3080 (Vulkan)");
    s.set_user_str(KnobField::Device, Some("ROCm0".into()));
    assert_eq!(s.device_value_display(), "AMD Radeon AI PRO R9700 (ROCm)");
  }

  #[test]
  fn device_value_display_unknown_selector_falls_back_to_raw() {
    // A persisted selector no longer in the catalog still renders
    // something useful rather than "default".
    let mut s = LaunchPickerState::for_model("test");
    s.device_catalog = catalog_two_vendors();
    s.set_user_str(KnobField::Device, Some("CUDA9".into()));
    assert_eq!(s.device_value_display(), "CUDA9");
  }

  #[test]
  fn cycle_device_walks_default_then_each_selector_and_wraps() {
    let mut s = LaunchPickerState::for_model("test");
    s.device_catalog = catalog_two_vendors();
    // Device is not fit-governed and `default` already means auto-select,
    // so there is no Auto stop. Ring: default → Vulkan0 → Vulkan1 →
    // ROCm0 → wrap.
    assert_eq!(s.user_value_str(KnobField::Device), None);
    s.cycle_device(KnobField::Device, true);
    assert_eq!(s.user_value_str(KnobField::Device), Some("Vulkan0".into()));
    assert!(
      !s.user_is_auto(KnobField::Device),
      "device has no Auto stop"
    );
    s.cycle_device(KnobField::Device, true);
    assert_eq!(s.user_value_str(KnobField::Device), Some("Vulkan1".into()));
    s.cycle_device(KnobField::Device, true);
    assert_eq!(s.user_value_str(KnobField::Device), Some("ROCm0".into()));
    // One more wraps back to default.
    s.cycle_device(KnobField::Device, true);
    assert_eq!(s.user_value_str(KnobField::Device), None);
    assert!(!s.user_is_auto(KnobField::Device));
  }

  #[test]
  fn cycle_device_backward_from_default_wraps_to_last() {
    let mut s = LaunchPickerState::for_model("test");
    s.device_catalog = catalog_two_vendors();
    s.cycle_device(KnobField::Device, false);
    assert_eq!(s.user_value_str(KnobField::Device), Some("ROCm0".into()));
    s.cycle_device(KnobField::Device, false);
    assert_eq!(s.user_value_str(KnobField::Device), Some("Vulkan1".into()));
  }

  #[test]
  fn cycle_device_stored_value_is_a_real_selector() {
    // Regression: the stored value must be a llama-server selector
    // (`Vulkan0`), never the old `card:driver` coordinate (`0:0`) that
    // made llama-server bail with `invalid device`.
    let mut s = LaunchPickerState::for_model("test");
    s.device_catalog = catalog_two_vendors();
    // Two steps past Inherited → Auto → the first real selector.
    s.cycle_device(KnobField::Device, true);
    s.cycle_device(KnobField::Device, true);
    let stored = s.user_value_str(KnobField::Device).unwrap().to_string();
    assert!(
      !stored.contains(':'),
      "selector must not be a coordinate: {stored}"
    );
    assert!(s.device_catalog.iter().any(|d| d.selector == stored));
  }

  #[test]
  fn cycle_device_with_empty_catalog_stays_default() {
    let mut s = LaunchPickerState::for_model("test");
    // Empty catalog (CPU-only / no binary) — cycling resets to default.
    s.cycle_device(KnobField::Device, true);
    assert_eq!(s.user_value_str(KnobField::Device), None);
    s.cycle_device(KnobField::Device, false);
    assert_eq!(s.user_value_str(KnobField::Device), None);
  }
}
