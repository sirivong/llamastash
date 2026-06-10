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
  /// Per-model backend choice (R17). The first row — it's the launch-level
  /// decision that gates which knobs below the active backend can honor.
  Backend,
  Knob(KnobField),
  Extras,
}

/// Lazily-built navigation order — every knob in editor **display**
/// order (the flattened [`knob_display_groups`], which clusters knobs
/// by function and is distinct from the pinned argv order), followed
/// by `Extras`. Built once on first access so per-keypress navigation
/// does no allocation.
static ALL_FIELDS: LazyLock<Box<[PickerField]>> = LazyLock::new(|| {
  // Backend leads — it's the launch-level choice that gates the knobs below.
  let mut v: Vec<PickerField> = vec![PickerField::Backend];
  for group in knob_display_groups() {
    for field in group.fields {
      v.push(PickerField::Knob(*field));
    }
  }
  v.push(PickerField::Extras);
  v.into_boxed_slice()
});

/// Backend choices for a GGUF (llama.cpp) model: `Auto` resolves to llama.cpp
/// via the identity rule, so the explicit entry is redundant and the chooser
/// row stays hidden (no cross-backend override — a local GGUF is not in any
/// other backend's registry).
const LLAMACPP_BACKEND_CHOICES: &[BackendChoice] = &[BackendChoice::Auto, BackendChoice::LlamaCpp];

/// Backend choices for a Lemonade-registry model: `Auto` resolves to Lemonade.
/// The chooser row is surfaced so the user sees the model launches via the
/// managed-multiplexer (and why only ctx + extras render below).
const LEMONADE_BACKEND_CHOICES: &[BackendChoice] = &[BackendChoice::Auto, BackendChoice::Lemonade];

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
      // Backend is cycled with ←/→ (a closed set), never typed.
      PickerField::Backend => false,
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
  /// Per-model backend choice (R17). `Auto` runs the identity rule (GGUF →
  /// llama.cpp); a managed-multiplexer backend greys the knob rows it can't
  /// honor.
  pub backend: BackendChoice,
  /// The focused model's *own* concrete backend (the one its catalog source
  /// binds to under `Auto`): `LlamaCpp` for a GGUF, `Lemonade` for a
  /// `lemonade://` registry model. Drives which choices the chooser offers and
  /// what `Auto` resolves to for knob-greying — never `Auto` itself.
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
      backend: BackendChoice::Auto,
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
      PickerField::Backend => self.cycle_backend(true),
      PickerField::Knob(k) => self.cycle_knob(k, true),
      PickerField::Extras => {}
    }
  }

  /// Cycle the focused field's value backward (Left arrow).
  pub fn cycle_focused_value_prev(&mut self) {
    match self.field {
      PickerField::Backend => self.cycle_backend(false),
      PickerField::Knob(k) => self.cycle_knob(k, false),
      PickerField::Extras => {}
    }
  }

  /// Backend choices the chooser cycles through, scoped to the focused
  /// model's own backend. A GGUF offers `[Auto, LlamaCpp]`; a Lemonade
  /// registry model offers `[Auto, Lemonade]`. There is no cross-backend
  /// override — a model lives in exactly one backend's catalog.
  fn backend_choices(&self) -> &'static [BackendChoice] {
    match self.model_backend {
      BackendChoice::Lemonade => LEMONADE_BACKEND_CHOICES,
      _ => LLAMACPP_BACKEND_CHOICES,
    }
  }

  /// Whether the backend chooser row appears. Shown for models whose backend
  /// isn't the default direct llama.cpp (i.e. Lemonade), so the user sees the
  /// non-default backend and the greyed knobs; hidden for plain GGUF rows
  /// where `Auto` and `llama.cpp` are the same and there's nothing to choose.
  fn backend_choice_available(&self) -> bool {
    matches!(self.model_backend, BackendChoice::Lemonade)
  }

  /// The backend that actually serves the launch: an explicit choice wins;
  /// `Auto` resolves to the model's own backend. Drives knob-greying so a
  /// Lemonade model under `Auto` correctly greys the llama.cpp knobs.
  fn effective_backend(&self) -> BackendChoice {
    match self.backend {
      BackendChoice::Auto => self.model_backend,
      other => other,
    }
  }

  /// Cycle the per-model backend choice through the model-scoped choices.
  fn cycle_backend(&mut self, forward: bool) {
    let choices = self.backend_choices();
    let i = choices.iter().position(|c| *c == self.backend).unwrap_or(0);
    let n = choices.len();
    let next = if forward {
      (i + 1) % n
    } else {
      (i + n - 1) % n
    };
    self.backend = choices[next];
  }

  /// Display label for the backend row value (`auto` / `llamacpp`).
  pub fn backend_label(&self) -> &'static str {
    self.backend.label()
  }

  /// Whether the resolved active backend honors `field` (R6).
  /// [`Self::field_visible`] hides rows the active backend can't honor.
  /// llama.cpp honors every typed knob; Lemonade honors `ctx` only
  /// (lemond's `ctx_size` load option). `Auto` resolves to the model's
  /// own backend first.
  pub fn knob_supported(&self, field: KnobField) -> bool {
    use crate::backend::lemonade::LemonadeBackend;
    use crate::backend::llama_cpp::LlamaCppBackend;
    use crate::backend::Backend;
    match self.effective_backend() {
      BackendChoice::Lemonade => LemonadeBackend::new().capabilities().supports(field),
      _ => LlamaCppBackend::new().capabilities().supports(field),
    }
  }

  /// The active backend's id for the "not supported by `<id>`" label.
  pub fn active_backend_id(&self) -> &'static str {
    match self.effective_backend() {
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

  /// Cycle a constrained-string knob (`cache_type_*`, `split_mode`)
  /// through its allowed `set`.
  fn cycle_str_set(&mut self, field: KnobField, set: &'static [&'static str], forward: bool) {
    // Find the current user-set value inside the `&'static [&'static str]`
    // catalog so cycle_through's `T = &'static str` lifetime detaches
    // from `&self` — that lets us call `set_user_str(&mut self, ...)`
    // immediately after without dragging a borrow. Avoids the prior
    // `Vec<String>` + `Vec<&str>` allocation pair on every keypress.
    let current: Option<&'static str> = self
      .user_value_str(field)
      .and_then(|s| set.iter().copied().find(|t| *t == s));
    let next = cycle_through(current, set, forward);
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

  /// Cycle the Device row through the flat catalog. The cycle space is
  /// `[default] + catalog selectors`: stepping off either end wraps
  /// back to `None` (default → auto-select). `user_knobs.device` holds
  /// the chosen selector verbatim (`"Vulkan1"`), or `None` for default.
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
    let current = self.user_value_str(field).filter(|s| !s.is_empty());
    // Current position in the cycle: None = default (index "0"),
    // Some(sel) = its catalog index + 1.
    let cur_pos: usize = match current {
      None => 0,
      Some(sel) => selectors
        .iter()
        .position(|s| *s == sel)
        .map(|i| i + 1)
        .unwrap_or(0),
    };
    let len = selectors.len() + 1; // +1 for the default slot
    let next_pos = if forward {
      (cur_pos + 1) % len
    } else {
      (cur_pos + len - 1) % len
    };
    let next = if next_pos == 0 {
      None
    } else {
      Some(selectors[next_pos - 1].to_string())
    };
    self.set_user_str(field, next);
  }

  /// Backspace on a focused row: clear the user override and re-
  /// inherit from the resolver chain.
  pub fn reset_focused_row(&mut self) {
    match self.field {
      // Backspace on the backend row resets the choice to Auto.
      PickerField::Backend => self.backend = BackendChoice::Auto,
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
  /// active backend can't honor are hidden outright (R6) — a Lemonade
  /// model shows just Backend, ctx, and extras rather than a column of
  /// dead rows. Delegates to the single-source group table.
  pub fn field_visible(&self, field: PickerField) -> bool {
    match field {
      // Backend chooser only when it adds information — i.e. the focused
      // model's backend isn't the default direct llama.cpp (R17). A GGUF row
      // hides + skips it; a Lemonade registry row surfaces it.
      PickerField::Backend => self.backend_choice_available(),
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
      KnobField::Ctx => self.user_knobs.ctx,
      KnobField::NGpuLayers => self.user_knobs.n_gpu_layers,
      KnobField::NCpuMoe => self.user_knobs.n_cpu_moe,
      KnobField::Threads => self.user_knobs.threads,
      KnobField::Parallel => self.user_knobs.parallel,
      KnobField::BatchSize => self.user_knobs.batch_size,
      KnobField::UbatchSize => self.user_knobs.ubatch_size,
      KnobField::Keep => self.user_knobs.keep,
      KnobField::MainGpu => self.user_knobs.main_gpu,
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
      KnobField::Device => self.user_knobs.device.as_deref(),
      KnobField::TensorSplit => self.user_knobs.tensor_split.as_deref(),
      KnobField::SplitMode => self.user_knobs.split_mode.as_deref(),
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
      KnobField::NCpuMoe => self.resolved.n_cpu_moe,
      KnobField::Threads => self.resolved.threads,
      KnobField::Parallel => self.resolved.parallel,
      KnobField::BatchSize => self.resolved.batch_size,
      KnobField::UbatchSize => self.resolved.ubatch_size,
      KnobField::Keep => self.resolved.keep,
      KnobField::MainGpu => self.resolved.main_gpu,
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
      KnobField::Device => self.resolved.device.as_deref(),
      KnobField::TensorSplit => self.resolved.tensor_split.as_deref(),
      KnobField::SplitMode => self.resolved.split_mode.as_deref(),
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
      self.user_knobs.rope_freq_scale = value;
    }
  }

  pub fn set_user_str(&mut self, field: KnobField, value: Option<String>) {
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
  fn gguf_model_hides_backend_row_and_offers_only_llamacpp() {
    // Default model_backend is LlamaCpp (a GGUF row).
    let s = LaunchPickerState::for_model("qwen");
    assert!(!s.backend_choice_available(), "GGUF hides the Backend row");
    assert_eq!(
      s.backend_choices(),
      &[BackendChoice::Auto, BackendChoice::LlamaCpp]
    );
    assert!(!s.field_visible(PickerField::Backend));
    // A GGUF under Auto honors llama.cpp knobs.
    assert!(s.knob_supported(KnobField::Ctx));
    assert_eq!(s.active_backend_id(), "llamacpp");
  }

  #[test]
  fn lemonade_model_shows_only_backend_ctx_and_extras() {
    let mut s = LaunchPickerState::for_model("Qwen2.5-7B");
    s.model_backend = BackendChoice::Lemonade;
    assert!(
      s.backend_choice_available(),
      "Lemonade surfaces the Backend row"
    );
    assert_eq!(
      s.backend_choices(),
      &[BackendChoice::Auto, BackendChoice::Lemonade]
    );
    assert!(s.field_visible(PickerField::Backend));
    // Under Auto a Lemonade model resolves to Lemonade, which honors
    // `ctx` (lemond's `ctx_size` load option) and the free-form extras
    // (`*_args`) — every other llama.cpp knob row is hidden outright.
    let visible: Vec<PickerField> = PickerField::all()
      .iter()
      .copied()
      .filter(|f| s.field_visible(*f))
      .collect();
    assert_eq!(
      visible,
      vec![
        PickerField::Backend,
        PickerField::Knob(KnobField::Ctx),
        PickerField::Extras,
      ],
      "lemonade picker is Backend + ctx + extras, nothing else"
    );
    assert_eq!(s.active_backend_id(), "lemonade");
  }

  #[test]
  fn cycle_backend_stays_within_the_models_backend() {
    // A GGUF can only ever cycle Auto <-> LlamaCpp; Lemonade is never offered.
    let mut s = LaunchPickerState::for_model("qwen");
    s.field = PickerField::Backend;
    assert_eq!(s.backend, BackendChoice::Auto);
    s.cycle_backend(true);
    assert_eq!(s.backend, BackendChoice::LlamaCpp);
    s.cycle_backend(true);
    assert_eq!(
      s.backend,
      BackendChoice::Auto,
      "wraps, never reaches Lemonade"
    );

    // A Lemonade model cycles Auto <-> Lemonade.
    let mut l = LaunchPickerState::for_model("Llama-3.1-8B");
    l.model_backend = BackendChoice::Lemonade;
    l.cycle_backend(true);
    assert_eq!(l.backend, BackendChoice::Lemonade);
    l.cycle_backend(true);
    assert_eq!(l.backend, BackendChoice::Auto);
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
    // Start at default (no override).
    assert_eq!(s.user_value_str(KnobField::Device), None);
    // Forward through every catalog selector in order.
    s.cycle_device(KnobField::Device, true);
    assert_eq!(s.user_value_str(KnobField::Device), Some("Vulkan0".into()));
    s.cycle_device(KnobField::Device, true);
    assert_eq!(s.user_value_str(KnobField::Device), Some("Vulkan1".into()));
    s.cycle_device(KnobField::Device, true);
    assert_eq!(s.user_value_str(KnobField::Device), Some("ROCm0".into()));
    // One more wraps back to default.
    s.cycle_device(KnobField::Device, true);
    assert_eq!(s.user_value_str(KnobField::Device), None);
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
