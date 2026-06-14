//! Compose `llama-server` argv from the user's launch choices.
//!
//! Order matters: `--host 127.0.0.1` and `--port` come first so the
//! command line reads well in logs; then `-m <path>`, then mode flags
//! (`--embeddings` / `--reranking`), then reasoning bundle
//! (`--jinja --reasoning-format deepseek`), then `-c <ctx>`, then
//! the typed knobs in canonical order, then any user-supplied
//! `extras` argv tail. `extras` land *last* so they always trump
//! everything else — that's the contract documented on the TUI's
//! "Settings" tab.
//!
//! `forbidden_in_extras` enforces the loopback-only and same-UID
//! contract: a curated denylist (`--host`, `--listen`, `--bind`,
//! `--api-key`, `--ssl-*`) is refused. llama-server honours the
//! last-occurrence of a flag, so without this guard a trailing
//! `--host 0.0.0.0` in `extras` would expose the model to the LAN.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::{DefaultLaunchMode, KnobValue, KnobValueOpt, TypedKnobs};
use crate::launch::flag_aliases::{knob_specs, KnobField, ValueKind};
use crate::launch::mode::LaunchMode;

/// Flags refused in `LaunchParams.extras` because they would break
/// the loopback-only / same-UID security contract documented in
/// `docs/architecture.md`. Match is case-insensitive on the flag
/// itself; `--ssl-*` matches any flag starting with that prefix.
pub const FORBIDDEN_ADVANCED_PREFIXES: &[&str] =
  &["--host", "--listen", "--bind", "--api-key", "--ssl-"];

fn is_forbidden_head(head: &str) -> bool {
  let lower = head.to_ascii_lowercase();
  FORBIDDEN_ADVANCED_PREFIXES
    .iter()
    .any(|p| lower == *p || (p.ends_with('-') && lower.starts_with(p)))
}

/// Flag heads whose adjacent value is a secret and must be hidden
/// before display in a log line, error message, or terminal echo.
/// Shared between [`forbidden_in_extras`] and [`redact_for_display`]
/// so both surfaces redact the same set.
const SECRET_BEARING_PREFIXES: &[&str] = &["--api-key", "--ssl-"];

fn is_secret_head(head: &str) -> bool {
  let lower = head.to_ascii_lowercase();
  SECRET_BEARING_PREFIXES
    .iter()
    .any(|p| lower == *p || (p.ends_with('-') && lower.starts_with(p)))
}

/// Returns the subset of `extras` flags that hit the denylist, with
/// secret-bearing values redacted (`--api-key=foo` → `--api-key=<value-redacted>`).
/// Callers must never display the *raw* extras list — only the
/// redacted strings returned here — so a typo'd secret can't land in
/// scrollback or daemon error logs.
///
/// Only the equals-form (`--api-key=foo`) needs explicit redaction
/// here: space-form values (`["--api-key", "foo"]`) arrive as their
/// own free-standing tokens, and `"foo"` on its own doesn't match
/// any forbidden head — so it's silently passed through this filter.
/// The launch is still refused on the basis of the `--api-key` head
/// alone, and the value never lands in the returned banned list.
/// `redact_for_display` does the peek-and-redact for space-form
/// because compose echoes the *full* extras tail back to the user.
pub fn forbidden_in_extras(extras: &[OsString]) -> Vec<String> {
  extras
    .iter()
    .filter_map(|s| {
      let lossy = s.to_string_lossy();
      let head = lossy.split('=').next().unwrap_or(&lossy);
      if !is_forbidden_head(head) {
        return None;
      }
      if is_secret_head(head) && lossy.contains('=') {
        Some(format!("{head}=<value-redacted>"))
      } else {
        Some(lossy.into_owned())
      }
    })
    .collect()
}

/// Format an extras list for human display, redacting values that
/// follow secret-bearing prefixes (`--api-key`, `--ssl-*`). Used by
/// the TUI's forbidden-flag inline warning and any other surface
/// that might echo extras back to a log or terminal.
pub fn redact_for_display(extras: &[OsString]) -> String {
  let is_secret = is_secret_head;
  let mut out = String::new();
  let mut iter = extras.iter().peekable();
  while let Some(token) = iter.next() {
    if !out.is_empty() {
      out.push(' ');
    }
    let lossy = token.to_string_lossy();
    if let Some((head, _value)) = lossy.split_once('=') {
      if is_secret(head) {
        out.push_str(head);
        out.push_str("=<value-redacted>");
        continue;
      }
    }
    out.push_str(&lossy);
    if !lossy.contains('=') && is_secret(&lossy) {
      if let Some(next) = iter.peek() {
        let next_lossy = next.to_string_lossy();
        if !next_lossy.starts_with('-') {
          out.push(' ');
          out.push_str("<value-redacted>");
          iter.next();
        }
      }
    }
  }
  out
}

/// Which inference backend should run a launch (R17).
///
/// This is a *launch-level* choice, not a translated knob — "which backend"
/// has no `llama-server` argv form, so it rides on [`LaunchParams`] rather
/// than [`TypedKnobs`]. The default [`BackendChoice::Auto`] runs the R13
/// identity rule (GGUF → llama.cpp, registry → its owning backend); an
/// explicit variant overrides it. Resolved by
/// [`crate::backend::resolve_backend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendChoice {
  /// Pick automatically from the model's identity (R13).
  #[default]
  Auto,
  /// Force the direct, zero-overhead llama.cpp backend. Wire value pinned
  /// to `"llamacpp"` so it matches the backend id + the `start --backend`
  /// flag (snake_case would give `llama_cpp`).
  #[serde(rename = "llamacpp")]
  LlamaCpp,
  /// Force the Lemonade (`lemond`) managed-multiplexer backend.
  Lemonade,
}

impl BackendChoice {
  /// Stable lowercase label for CLI parsing / JSON projection.
  pub fn label(self) -> &'static str {
    match self {
      BackendChoice::Auto => "auto",
      BackendChoice::LlamaCpp => "llamacpp",
      BackendChoice::Lemonade => "lemonade",
    }
  }
}

/// All launch knobs the supervisor reads. Persisted under
/// `last_params: HashMap<ModelIdentity, LaunchParams>` in `state.json`.
///
/// Pre-1.0 schema flip: the old `advanced: Vec<OsString>` field has
/// been replaced with `knobs: TypedKnobs` + `extras: Vec<OsString>`.
/// Existing state files from before the flip parse-fail and
/// quarantine to `state.json.broken-<ts>` per `daemon::mod`'s
/// existing path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaunchParams {
  /// Absolute path to the GGUF the user picked (or shard 1 for split
  /// sets).
  pub model_path: PathBuf,
  /// Chosen launch mode (chat / embedding / rerank).
  pub mode: LaunchMode,
  /// Context length. `None` lets `llama-server` use the GGUF's
  /// native value (no `-c` flag).
  ///
  /// **Persistence note:** on a running launch this holds the
  /// *resolved* ctx the supervisor argv-ified (after the
  /// `user > last_used > arch_defaults > builtin > model_default`
  /// chain). It may differ from `knobs.ctx`, which holds the
  /// *user-supplied delta* — the field the editor seeds `user_knobs`
  /// from on return. Read `knobs.ctx` for source-chip semantics;
  /// read this for what actually shipped on the wire.
  pub ctx: Option<u32>,
  /// Listening port. `None` leaves port allocation to the supervisor.
  pub port: Option<u16>,
  /// `--fit-ctx` floor, emitted **only when `ctx` is unset** (Auto /
  /// Inherited) so `--fit` never collapses the window below this size.
  /// A pinned `ctx` wins and suppresses `--fit-ctx` (fit honors the
  /// pin). `None` emits no floor. Sourced from `LaunchEnv.fit_ctx_floor`
  /// by `start_model`. `#[serde(default)]` keeps older `state.json` rows
  /// loading.
  #[serde(default)]
  pub fit_ctx_floor: Option<u32>,
  /// Reasoning bundle on/off. When `true`, supervisor appends
  /// `--jinja --reasoning-format deepseek` to the argv.
  ///
  /// **Persistence note:** like `ctx` above, this is the *resolved*
  /// value collapsed to a bool (`None`/`Some(false)` → `false`).
  /// May differ from `knobs.reasoning`, which keeps the tri-state
  /// `Option<bool>` the user actually supplied.
  pub reasoning: bool,
  /// Resolved typed knobs — argvified before `extras` in canonical
  /// flag order. `None`-fields are skipped (no flag emitted).
  #[serde(default)]
  pub knobs: TypedKnobs,
  /// Free-form argv tail for `llama-server` flags the typed editor
  /// doesn't model (e.g. `--rope-freq-base`, sampling params).
  /// Emitted *after* `knobs` so the last-occurrence wins per
  /// llama-server semantics — same "extras trump bundled" contract
  /// documented on the Settings tab.
  #[serde(default)]
  pub extras: Vec<OsString>,
  /// Optional path to a multimodal projector (mmproj) file. When set,
  /// the supervisor appends `--mmproj <path>` to the llama-server
  /// argv. The file is auto-detected by scanning the parent directory
  /// of the model for a `mmproj-<stem>.gguf` or `mmproj_<stem>.gguf`
  /// companion.
  #[serde(default)]
  pub mmproj_path: Option<PathBuf>,
  /// Which backend runs this launch (R17). Defaults to
  /// [`BackendChoice::Auto`] (the R13 identity rule); an explicit value
  /// overrides per-model. Persisted in last-params like the other choices,
  /// so a returning user keeps their override. `#[serde(default)]` keeps
  /// pre-Phase-2b `state.json` rows loading as `Auto`.
  #[serde(default)]
  pub backend: BackendChoice,
}

impl LaunchParams {
  pub fn new(model_path: PathBuf, mode: LaunchMode) -> Self {
    Self {
      model_path,
      mode,
      ctx: None,
      port: None,
      fit_ctx_floor: None,
      reasoning: false,
      knobs: TypedKnobs::default(),
      extras: Vec::new(),
      mmproj_path: None,
      backend: BackendChoice::default(),
    }
  }
}

/// Argv-ify the typed knob set in canonical flag order. Skips
/// `None` fields; for booleans, only emits the flag when
/// `Some(true)` (`Some(false)` is an explicit opt-out — no
/// `--no-flash-attn` form because llama-server doesn't have one).
///
/// `Ctx` and `Reasoning` are deliberately skipped here — they live
/// in `TypedKnobs` for the resolver chain and the editor's source
/// chips, but `compose` emits them inline (ctx → `-c <N>`, reasoning
/// → `--jinja --reasoning-format deepseek`) so their argv order and
/// bundle shape stay distinct from the other knobs.
///
/// `Device` is also skipped here: `knobs.device` holds a real
/// `llama-server` device selector (`Vulkan0`, `CUDA0`, `ROCm0`) and
/// `compose` emits it exactly once as `--device <selector>`. Emitting
/// it here too would put a *second* `--device` on the argv;
/// llama-server validates each `--device` token as it parses, so a
/// stray/duplicate value makes it bail with `invalid device: …` before
/// last-occurrence-wins ever applies.
pub fn argvify(knobs: &TypedKnobs) -> Vec<OsString> {
  let mut out: Vec<OsString> = Vec::new();
  for spec in knob_specs() {
    match spec.field {
      // Skipped here (emitted inline by `compose`) or governed by fit:
      // an `Auto` knob falls through `set_value()` to `None`, so no
      // flag is emitted and `--fit` is left to place it.
      KnobField::Ctx | KnobField::Reasoning | KnobField::Device => continue,
      KnobField::NGpuLayers => push_u32(
        &mut out,
        spec.canonical,
        knobs.n_gpu_layers.set_value().copied(),
      ),
      KnobField::NCpuMoe => push_u32(
        &mut out,
        spec.canonical,
        knobs.n_cpu_moe.set_value().copied(),
      ),
      KnobField::Threads => push_u32(&mut out, spec.canonical, knobs.threads.set_value().copied()),
      KnobField::CacheTypeK => push_str(
        &mut out,
        spec.canonical,
        knobs.cache_type_k.set_value().map(String::as_str),
      ),
      KnobField::CacheTypeV => push_str(
        &mut out,
        spec.canonical,
        knobs.cache_type_v.set_value().map(String::as_str),
      ),
      KnobField::Parallel => push_u32(
        &mut out,
        spec.canonical,
        knobs.parallel.set_value().copied(),
      ),
      KnobField::FlashAttn => push_flash_attn(
        &mut out,
        spec.canonical,
        knobs.flash_attn.set_value().copied(),
      ),
      KnobField::Mlock => push_bool(&mut out, spec.canonical, knobs.mlock.set_value().copied()),
      KnobField::NoMmap => push_bool(&mut out, spec.canonical, knobs.no_mmap.set_value().copied()),
      KnobField::BatchSize => push_u32(
        &mut out,
        spec.canonical,
        knobs.batch_size.set_value().copied(),
      ),
      KnobField::UbatchSize => push_u32(
        &mut out,
        spec.canonical,
        knobs.ubatch_size.set_value().copied(),
      ),
      KnobField::RopeFreqScale => push_f32(
        &mut out,
        spec.canonical,
        knobs.rope_freq_scale.set_value().copied(),
      ),
      KnobField::Keep => push_u32(&mut out, spec.canonical, knobs.keep.set_value().copied()),
      KnobField::TensorSplit => push_str(
        &mut out,
        spec.canonical,
        knobs.tensor_split.set_value().map(String::as_str),
      ),
      KnobField::MainGpu => push_u32(
        &mut out,
        spec.canonical,
        knobs.main_gpu.set_value().copied(),
      ),
      KnobField::SplitMode => push_str(
        &mut out,
        spec.canonical,
        knobs.split_mode.set_value().map(String::as_str),
      ),
    }
    // `ValueKind` is the source-of-truth for emission shape; sanity
    // check that our match handled the right kind.
    debug_assert!(
      matches!(
        spec.kind,
        ValueKind::U32
          | ValueKind::F32
          | ValueKind::Bool
          | ValueKind::KvCacheType
          | ValueKind::SplitMode
          | ValueKind::Str
      ),
      "ValueKind exhaustiveness drift"
    );
  }
  out
}

fn push_u32(out: &mut Vec<OsString>, canonical: &str, value: Option<u32>) {
  if let Some(v) = value {
    out.push(canonical.into());
    out.push(v.to_string().into());
  }
}

fn push_f32(out: &mut Vec<OsString>, canonical: &str, value: Option<f32>) {
  if let Some(v) = value {
    out.push(canonical.into());
    out.push(format_f32(v).into());
  }
}

fn push_str(out: &mut Vec<OsString>, canonical: &str, value: Option<&str>) {
  if let Some(v) = value {
    out.push(canonical.into());
    out.push(v.to_string().into());
  }
}

fn push_bool(out: &mut Vec<OsString>, canonical: &str, value: Option<bool>) {
  if value == Some(true) {
    out.push(canonical.into());
  }
}

/// Modern llama-server (b9000+) requires `--flash-attn on|off|auto`
/// and rejects the bare flag — passing `--flash-attn` alone causes
/// the next argv entry to be parsed as the flash-attn value.
fn push_flash_attn(out: &mut Vec<OsString>, canonical: &str, value: Option<bool>) {
  match value {
    Some(true) => {
      out.push(canonical.into());
      out.push("on".into());
    }
    Some(false) => {
      out.push(canonical.into());
      out.push("off".into());
    }
    None => {}
  }
}

/// Format an f32 without trailing zeros beyond the canonical
/// representation. Integer-valued floats render with a `.0` suffix
/// so the value still reads as a float (e.g. `2` → `"2.0"`).
fn format_f32(v: f32) -> String {
  if v.fract() == 0.0 && v.is_finite() {
    format!("{v:.1}")
  } else {
    format!("{v}")
  }
}

/// One layer in the precedence chain (R106). The label is reported
/// back in `Resolved.sources` so the editor can render per-row
/// origin chips (`(user)`, `(last used)`, `(arch default)`,
/// `(model default)`, `(server default)`).
///
/// `ArchDefault` covers both the user's yaml `arch_defaults` block
/// and the compiled-in arch table — yaml wins per field at resolve
/// time, but the chip is the same since both are conceptually
/// "what this arch defaults to."
///
/// `ModelDefault` means the value comes from the model file itself
/// (GGUF header for `ctx`, chat template for `reasoning`).
/// `ServerDefault` means no flag is sent and llama-server falls back
/// to its own hardcoded default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LayerLabel {
  User,
  LastUsed,
  ArchDefault,
  ModelDefault,
  ServerDefault,
}

impl LayerLabel {
  /// Human-readable, single-token label rendered in the editor.
  pub fn label(self) -> &'static str {
    match self {
      LayerLabel::User => "user",
      LayerLabel::LastUsed => "last used",
      LayerLabel::ArchDefault => "arch default",
      LayerLabel::ModelDefault => "model default",
      LayerLabel::ServerDefault => "server default",
    }
  }
}

/// Resolver output. `knobs` is the merged set the supervisor uses;
/// `sources` names which layer contributed each field so the editor
/// can render origin chips. Fields the resolver couldn't fill from
/// any layer land on `LayerLabel::ModelDefault` in `sources`.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
  pub knobs: TypedKnobs,
  pub sources: BTreeMap<KnobField, LayerLabel>,
}

/// Walk `layers` top-down per field; the first `Some` wins. Each
/// layer contributes a `LayerLabel` so the resulting `Resolved`
/// names where every field came from.
///
/// Layers are passed in precedence order — most-specific first. The
/// IPC handler builds `[(User, &caller_knobs), (LastUsed, &last),
/// (ArchDefault, &yaml), (ArchDefault, &table_lookup)]` — yaml and
/// the compiled-in arch table share the `ArchDefault` chip label,
/// with yaml winning per-field via precedence order. Anything still
/// `None` after that walk is annotated with the field's
/// `spec.fallback_label` — `ModelDefault` for ctx/reasoning (read
/// from the model file when omitted), `ServerDefault` for everything
/// else (llama-server's hardcoded default).
///
/// When `LLAMASTASH_BENCH_DISABLE_DEFAULTS=1` is set in the
/// environment, the resolver collapses to "User-labeled layers only"
/// — preset, last-used, yaml-arch, and compiled-in arch defaults are
/// all skipped. The benchmark harness sets this to make
/// `llamastash start` produce byte-identical argv to raw
/// `llama-server` for the same explicit knobs. Documented as
/// maintainer / bench-internal; not a public knob.
pub fn resolve_layered(layers: &[(LayerLabel, &TypedKnobs)]) -> Resolved {
  resolve_layered_with_disable_defaults(layers, bench_disable_defaults_from_env())
}

/// Inner resolver used by [`resolve_layered`]. Split out so tests
/// can exercise the bench-disable-defaults branch without mutating
/// process environment (env-var mutation in tests is racy across
/// `cargo test`'s thread pool).
pub fn resolve_layered_with_disable_defaults(
  layers: &[(LayerLabel, &TypedKnobs)],
  disable_defaults: bool,
) -> Resolved {
  if disable_defaults {
    let user_only: Vec<(LayerLabel, &TypedKnobs)> = layers
      .iter()
      .filter(|(l, _)| matches!(l, LayerLabel::User))
      .copied()
      .collect();
    return resolve_layered_inner(&user_only);
  }
  resolve_layered_inner(layers)
}

fn resolve_layered_inner(layers: &[(LayerLabel, &TypedKnobs)]) -> Resolved {
  let mut knobs = TypedKnobs::default();
  let mut sources: BTreeMap<KnobField, LayerLabel> = BTreeMap::new();
  for spec in knob_specs() {
    sources.insert(spec.field, spec.fallback_label);
  }
  for spec in knob_specs() {
    for (label, layer) in layers {
      if try_inherit_field(&mut knobs, layer, spec.field) {
        sources.insert(spec.field, *label);
        break;
      }
    }
  }
  Resolved { knobs, sources }
}

/// Seed every knob no layer filled — its source is still the
/// `fallback_label`, meaning no User / LastUsed / ArchDefault layer
/// supplied it — per the configured default launch mode (R1 seeding
/// rule). Under [`DefaultLaunchMode::Auto`] those layer-less knobs
/// become [`KnobValue::Auto`] so `--fit` governs them; under
/// `Inherited` they stay `None` and fall through to llama-server's own
/// default. Knobs a real layer supplied are left untouched, so
/// *remembered values win*.
///
/// `fallback_label` is `ServerDefault`/`ModelDefault` — labels no real
/// layer ever carries — so `source == fallback_label` is an exact test
/// for "layer-less".
pub fn seed_layerless(resolved: &mut Resolved, mode: DefaultLaunchMode) {
  if mode != DefaultLaunchMode::Auto {
    return;
  }
  for spec in knob_specs() {
    // Only fit-governed knobs get the Auto seed: for everything else
    // Auto is a no-op (emits nothing, same as Inherited) and would just
    // render a meaningless `auto` row. Non-fit layer-less knobs stay
    // Inherited and fall through to the server default.
    if !spec.field.fit_governed() {
      continue;
    }
    let layer_less = resolved
      .sources
      .get(&spec.field)
      .copied()
      .map(|src| src == spec.fallback_label)
      .unwrap_or(true);
    if layer_less {
      set_field_auto(&mut resolved.knobs, spec.field);
    }
  }
}

/// True when the knob slot is explicitly [`KnobValue::Auto`], keyed by
/// field. The counterpart to [`set_field_auto`]; used by the TUI picker
/// to render/cycle the Auto stop.
pub fn field_is_auto(knobs: &TypedKnobs, field: KnobField) -> bool {
  match field {
    KnobField::Ctx => knobs.ctx.is_auto(),
    KnobField::Reasoning => knobs.reasoning.is_auto(),
    KnobField::NGpuLayers => knobs.n_gpu_layers.is_auto(),
    KnobField::NCpuMoe => knobs.n_cpu_moe.is_auto(),
    KnobField::Threads => knobs.threads.is_auto(),
    KnobField::CacheTypeK => knobs.cache_type_k.is_auto(),
    KnobField::CacheTypeV => knobs.cache_type_v.is_auto(),
    KnobField::FlashAttn => knobs.flash_attn.is_auto(),
    KnobField::Mlock => knobs.mlock.is_auto(),
    KnobField::NoMmap => knobs.no_mmap.is_auto(),
    KnobField::Parallel => knobs.parallel.is_auto(),
    KnobField::BatchSize => knobs.batch_size.is_auto(),
    KnobField::UbatchSize => knobs.ubatch_size.is_auto(),
    KnobField::RopeFreqScale => knobs.rope_freq_scale.is_auto(),
    KnobField::Keep => knobs.keep.is_auto(),
    KnobField::Device => knobs.device.is_auto(),
    KnobField::TensorSplit => knobs.tensor_split.is_auto(),
    KnobField::MainGpu => knobs.main_gpu.is_auto(),
    KnobField::SplitMode => knobs.split_mode.is_auto(),
  }
}

/// Set a single knob slot to [`KnobValue::Auto`], keyed by field. Used
/// by the seeding rule and by the CLI `auto` literal parser.
pub fn set_field_auto(knobs: &mut TypedKnobs, field: KnobField) {
  match field {
    KnobField::Ctx => knobs.ctx = Some(KnobValue::Auto),
    KnobField::Reasoning => knobs.reasoning = Some(KnobValue::Auto),
    KnobField::NGpuLayers => knobs.n_gpu_layers = Some(KnobValue::Auto),
    KnobField::NCpuMoe => knobs.n_cpu_moe = Some(KnobValue::Auto),
    KnobField::Threads => knobs.threads = Some(KnobValue::Auto),
    KnobField::CacheTypeK => knobs.cache_type_k = Some(KnobValue::Auto),
    KnobField::CacheTypeV => knobs.cache_type_v = Some(KnobValue::Auto),
    KnobField::FlashAttn => knobs.flash_attn = Some(KnobValue::Auto),
    KnobField::Mlock => knobs.mlock = Some(KnobValue::Auto),
    KnobField::NoMmap => knobs.no_mmap = Some(KnobValue::Auto),
    KnobField::Parallel => knobs.parallel = Some(KnobValue::Auto),
    KnobField::BatchSize => knobs.batch_size = Some(KnobValue::Auto),
    KnobField::UbatchSize => knobs.ubatch_size = Some(KnobValue::Auto),
    KnobField::RopeFreqScale => knobs.rope_freq_scale = Some(KnobValue::Auto),
    KnobField::Keep => knobs.keep = Some(KnobValue::Auto),
    KnobField::Device => knobs.device = Some(KnobValue::Auto),
    KnobField::TensorSplit => knobs.tensor_split = Some(KnobValue::Auto),
    KnobField::MainGpu => knobs.main_gpu = Some(KnobValue::Auto),
    KnobField::SplitMode => knobs.split_mode = Some(KnobValue::Auto),
  }
}

/// Strict-`"1"` env-var read for `LLAMASTASH_BENCH_DISABLE_DEFAULTS`.
/// Any other value (including `"0"`, `"true"`, `"yes"`, empty
/// string, or unset) is treated as "not set." This matches the
/// existing `LLAMASTASH_ASSUME_NON_TTY` pattern in
/// `src/init/prompts.rs` so users have a consistent contract across
/// the bench-internal env vars.
fn bench_disable_defaults_from_env() -> bool {
  std::env::var_os("LLAMASTASH_BENCH_DISABLE_DEFAULTS").is_some_and(|v| v == "1")
}

/// If `field` is `Some` on `from` and `None` on `into`, copy it.
/// Returns true when a copy happened.
fn try_inherit_field(into: &mut TypedKnobs, from: &TypedKnobs, field: KnobField) -> bool {
  match field {
    KnobField::Ctx => copy_some(&mut into.ctx, from.ctx),
    KnobField::Reasoning => copy_some(&mut into.reasoning, from.reasoning),
    KnobField::NGpuLayers => copy_some(&mut into.n_gpu_layers, from.n_gpu_layers),
    KnobField::NCpuMoe => copy_some(&mut into.n_cpu_moe, from.n_cpu_moe),
    KnobField::Threads => copy_some(&mut into.threads, from.threads),
    KnobField::CacheTypeK => copy_some_clone(&mut into.cache_type_k, &from.cache_type_k),
    KnobField::CacheTypeV => copy_some_clone(&mut into.cache_type_v, &from.cache_type_v),
    KnobField::FlashAttn => copy_some(&mut into.flash_attn, from.flash_attn),
    KnobField::Mlock => copy_some(&mut into.mlock, from.mlock),
    KnobField::NoMmap => copy_some(&mut into.no_mmap, from.no_mmap),
    KnobField::Parallel => copy_some(&mut into.parallel, from.parallel),
    KnobField::BatchSize => copy_some(&mut into.batch_size, from.batch_size),
    KnobField::UbatchSize => copy_some(&mut into.ubatch_size, from.ubatch_size),
    KnobField::RopeFreqScale => copy_some(&mut into.rope_freq_scale, from.rope_freq_scale),
    KnobField::Keep => copy_some(&mut into.keep, from.keep),
    KnobField::Device => copy_some_clone(&mut into.device, &from.device),
    KnobField::TensorSplit => copy_some_clone(&mut into.tensor_split, &from.tensor_split),
    KnobField::MainGpu => copy_some(&mut into.main_gpu, from.main_gpu),
    KnobField::SplitMode => copy_some_clone(&mut into.split_mode, &from.split_mode),
  }
}

/// Inherit a knob slot wholesale — `Set` *and* `Auto` ride through
/// unchanged, since each is a distinct state a lower layer can supply.
/// Only the absent state (`None` = Inherited) falls through to the next
/// layer.
fn copy_some<T: Copy>(into: &mut Option<KnobValue<T>>, from: Option<KnobValue<T>>) -> bool {
  if into.is_none() {
    if let Some(v) = from {
      *into = Some(v);
      return true;
    }
  }
  false
}

fn copy_some_clone(into: &mut Option<KnobValue<String>>, from: &Option<KnobValue<String>>) -> bool {
  if into.is_none() {
    if let Some(v) = from {
      *into = Some(v.clone());
      return true;
    }
  }
  false
}

/// Materialise the argv `Command::args(...)` will hand to
/// `llama-server`. Caller passes the resolved listening port
/// separately because allocation happens in the supervisor, not in
/// `LaunchParams`.
///
/// `params.knobs.device`, when set, is a real `llama-server` device
/// selector (`Vulkan0`, `CUDA0`, `ROCm0`) sourced from that binary's
/// own `--list-devices` output (see [`crate::launch::list_devices`]).
/// It is emitted verbatim as a single `--device <selector>` — no index
/// math, no backend guessing. The caller is responsible for spawning
/// the matching binary so the selector is valid.
pub fn compose(params: &LaunchParams, allocated_port: u16) -> Vec<OsString> {
  let mut knob_argv = argvify(&params.knobs);
  let mut argv: Vec<OsString> = Vec::with_capacity(16 + knob_argv.len() + params.extras.len());
  argv.push("--host".into());
  argv.push("127.0.0.1".into());
  argv.push("--port".into());
  argv.push(allocated_port.to_string().into());
  argv.push("-m".into());
  argv.push(params.model_path.clone().into());
  if let Some(ref mmproj) = params.mmproj_path {
    argv.push("--mmproj".into());
    argv.push(mmproj.clone().into());
  }
  match params.mode {
    LaunchMode::Chat => {}
    LaunchMode::Embedding => argv.push("--embeddings".into()),
    LaunchMode::Rerank => argv.push("--reranking".into()),
  }
  if params.reasoning {
    argv.push("--jinja".into());
    argv.push("--reasoning-format".into());
    argv.push("deepseek".into());
  }
  // Context window: a pinned `ctx` emits `-c <N>` and suppresses
  // `--fit-ctx` (fit honors the pin). An unset `ctx` (Auto / Inherited)
  // emits `--fit-ctx <floor>` so `--fit` sizes the window for the
  // available memory but never collapses below the floor.
  if let Some(ctx) = params.ctx {
    argv.push("-c".into());
    argv.push(ctx.to_string().into());
  } else if let Some(floor) = params.fit_ctx_floor {
    argv.push("--fit-ctx".into());
    argv.push(floor.to_string().into());
  }
  // Emit the device selector verbatim — exactly once. Empty / unset
  // means "let llama-server auto-select" (no flag).
  if let Some(sel) = params
    .knobs
    .device
    .set_value()
    .map(String::as_str)
    .filter(|s| !s.is_empty())
  {
    knob_argv.push("--device".into());
    knob_argv.push(sel.into());
  }
  argv.extend(knob_argv);
  // Defensive strip: refuse to pass loopback-breaking flags even if
  // an upstream validator was skipped. Last-occurrence semantics in
  // llama-server mean a single `--host 0.0.0.0` here would override
  // the bundled `--host 127.0.0.1` above.
  let mut iter = params.extras.iter().peekable();
  while let Some(adv) = iter.next() {
    let lossy = adv.to_string_lossy();
    let head = lossy
      .split('=')
      .next()
      .unwrap_or(&lossy)
      .to_ascii_lowercase();
    if is_forbidden_head(&head) {
      log::warn!("compose: stripping forbidden extras flag {lossy:?}");
      if !lossy.contains('=') {
        if let Some(next) = iter.peek() {
          let next_lossy = next.to_string_lossy();
          if !next_lossy.starts_with('-') {
            iter.next();
          }
        }
      }
      continue;
    }
    argv.push(adv.clone());
  }
  argv
}

#[cfg(test)]
mod tests {
  use super::*;

  fn strs(args: &[OsString]) -> Vec<String> {
    args
      .iter()
      .map(|s| s.to_string_lossy().into_owned())
      .collect()
  }

  fn base_params() -> LaunchParams {
    LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat)
  }

  #[test]
  fn launch_params_defaults_backend_to_auto() {
    assert_eq!(base_params().backend, BackendChoice::Auto);
  }

  #[test]
  fn launch_params_without_backend_field_loads_as_auto() {
    // A pre-Phase-2b last_params row has no `backend` key; #[serde(default)]
    // must load it as Auto so existing state.json keeps working (R17).
    let mut v = serde_json::to_value(base_params()).unwrap();
    v.as_object_mut().unwrap().remove("backend");
    assert!(v.get("backend").is_none());
    let p: LaunchParams = serde_json::from_value(v).unwrap();
    assert_eq!(p.backend, BackendChoice::Auto);
  }

  #[test]
  fn backend_choice_serde_is_snake_case() {
    for c in [
      BackendChoice::Auto,
      BackendChoice::LlamaCpp,
      BackendChoice::Lemonade,
    ] {
      let s = serde_json::to_string(&c).unwrap();
      let back: BackendChoice = serde_json::from_str(&s).unwrap();
      assert_eq!(c, back);
    }
    assert_eq!(
      serde_json::to_string(&BackendChoice::LlamaCpp).unwrap(),
      "\"llamacpp\""
    );
    assert_eq!(
      serde_json::to_string(&BackendChoice::Lemonade).unwrap(),
      "\"lemonade\""
    );
  }

  #[test]
  fn chat_mode_emits_canonical_argv_prefix() {
    let p = base_params();
    let argv = strs(&compose(&p, 41100));
    let head: Vec<&str> = argv.iter().map(String::as_str).take(6).collect();
    assert_eq!(
      head,
      vec![
        "--host",
        "127.0.0.1",
        "--port",
        "41100",
        "-m",
        "/m/model.gguf"
      ]
    );
    assert!(!argv
      .iter()
      .any(|a| a == "--embeddings" || a == "--reranking"));
  }

  #[test]
  fn unset_ctx_emits_fit_ctx_floor_and_no_ngl() {
    // Auto / Inherited ctx (None) + a configured floor → `--fit-ctx`,
    // no `-c`, and (after de-pin) no `-ngl`.
    let mut p = base_params();
    p.ctx = None;
    p.fit_ctx_floor = Some(16384);
    let argv = strs(&compose(&p, 41100));
    let pos = argv
      .iter()
      .position(|a| a == "--fit-ctx")
      .expect("--fit-ctx");
    assert_eq!(argv[pos + 1], "16384");
    assert!(!argv.iter().any(|a| a == "-c"), "no -c when ctx unset");
    assert!(!argv.iter().any(|a| a == "-ngl"), "ngl is de-pinned");
  }

  #[test]
  fn pinned_ctx_emits_dash_c_and_suppresses_fit_ctx() {
    // A user-pinned ctx wins: `-c <N>`, no `--fit-ctx` (fit honors it).
    let mut p = base_params();
    p.ctx = Some(32768);
    p.fit_ctx_floor = Some(16384);
    let argv = strs(&compose(&p, 41100));
    let pos = argv.iter().position(|a| a == "-c").expect("-c");
    assert_eq!(argv[pos + 1], "32768");
    assert!(
      !argv.iter().any(|a| a == "--fit-ctx"),
      "a pinned ctx suppresses the fit floor"
    );
  }

  #[test]
  fn unset_ctx_without_floor_emits_neither() {
    // No floor configured (e.g. a bare LaunchParams) → no ctx flags.
    let p = base_params();
    let argv = strs(&compose(&p, 41100));
    assert!(!argv.iter().any(|a| a == "-c" || a == "--fit-ctx"));
  }

  #[test]
  fn embedding_mode_adds_embeddings_flag() {
    let mut p = base_params();
    p.mode = LaunchMode::Embedding;
    let argv = strs(&compose(&p, 41100));
    assert!(argv.iter().any(|a| a == "--embeddings"));
    assert!(!argv.iter().any(|a| a == "--reranking"));
  }

  #[test]
  fn rerank_mode_adds_reranking_flag() {
    let mut p = base_params();
    p.mode = LaunchMode::Rerank;
    let argv = strs(&compose(&p, 41100));
    assert!(argv.iter().any(|a| a == "--reranking"));
  }

  #[test]
  fn reasoning_bundles_jinja_and_deepseek() {
    let mut p = base_params();
    p.reasoning = true;
    let argv = strs(&compose(&p, 41100));
    assert!(argv.iter().any(|a| a == "--jinja"));
    let i = argv.iter().position(|a| a == "--reasoning-format").unwrap();
    assert_eq!(argv[i + 1], "deepseek");
  }

  #[test]
  fn ctx_override_emits_dash_c() {
    let mut p = base_params();
    p.ctx = Some(32768);
    let argv = strs(&compose(&p, 41100));
    let i = argv.iter().position(|a| a == "-c").unwrap();
    assert_eq!(argv[i + 1], "32768");
  }

  #[test]
  fn ctx_unset_omits_dash_c() {
    let p = base_params();
    let argv = strs(&compose(&p, 41100));
    assert!(!argv.iter().any(|a| a == "-c"));
  }

  #[test]
  fn argvify_emits_full_set_in_canonical_order() {
    let knobs = TypedKnobs {
      ctx: Some(KnobValue::Set(32768)),
      reasoning: Some(KnobValue::Set(true)),
      n_gpu_layers: Some(KnobValue::Set(99)),
      n_cpu_moe: Some(KnobValue::Set(12)),
      threads: Some(KnobValue::Set(8)),
      cache_type_k: Some(KnobValue::Set("q8_0".into())),
      cache_type_v: Some(KnobValue::Set("q8_0".into())),
      flash_attn: Some(KnobValue::Set(true)),
      mlock: Some(KnobValue::Set(true)),
      no_mmap: Some(KnobValue::Set(true)),
      parallel: Some(KnobValue::Set(4)),
      batch_size: Some(KnobValue::Set(2048)),
      ubatch_size: Some(KnobValue::Set(512)),
      rope_freq_scale: Some(KnobValue::Set(1.0)),
      keep: Some(KnobValue::Set(128)),
      device: None,
      tensor_split: Some(KnobValue::Set("3,1".into())),
      main_gpu: Some(KnobValue::Set(0)),
      split_mode: Some(KnobValue::Set("layer".into())),
    };
    let argv = strs(&argvify(&knobs));
    assert_eq!(
      argv,
      vec![
        "--n-gpu-layers",
        "99",
        "--n-cpu-moe",
        "12",
        "--tensor-split",
        "3,1",
        "--main-gpu",
        "0",
        "--split-mode",
        "layer",
        "--threads",
        "8",
        "--cache-type-k",
        "q8_0",
        "--cache-type-v",
        "q8_0",
        "--parallel",
        "4",
        "--flash-attn",
        "on",
        "--mlock",
        "--no-mmap",
        "--batch-size",
        "2048",
        "--ubatch-size",
        "512",
        "--rope-freq-scale",
        "1.0",
        "--keep",
        "128",
      ]
    );
  }

  #[test]
  fn argvify_skips_none_fields() {
    let knobs = TypedKnobs {
      n_gpu_layers: Some(KnobValue::Set(99)),
      flash_attn: Some(KnobValue::Set(true)),
      ..TypedKnobs::default()
    };
    let argv = strs(&argvify(&knobs));
    assert_eq!(argv, vec!["--n-gpu-layers", "99", "--flash-attn", "on"]);
  }

  #[test]
  fn argvify_some_false_omits_bare_bool_flags() {
    // True bare flags (`--mlock`, `--no-mmap`) are absent when set to
    // false — there's no `--no-mlock` form in llama-server.
    let knobs = TypedKnobs {
      mlock: Some(KnobValue::Set(false)),
      no_mmap: Some(KnobValue::Set(false)),
      ..TypedKnobs::default()
    };
    let argv = strs(&argvify(&knobs));
    assert!(
      argv.is_empty(),
      "Some(false) bare bools must not emit the flag"
    );
  }

  #[test]
  fn argvify_flash_attn_false_emits_off() {
    // `--flash-attn` takes a value (`on|off|auto`); Some(false) MUST
    // emit `--flash-attn off` so a user override actually disables it
    // when an inherited layer set Some(true).
    let knobs = TypedKnobs {
      flash_attn: Some(KnobValue::Set(false)),
      ..TypedKnobs::default()
    };
    let argv = strs(&argvify(&knobs));
    assert_eq!(argv, vec!["--flash-attn", "off"]);
  }

  #[test]
  fn argvify_empty_yields_empty() {
    let argv = strs(&argvify(&TypedKnobs::default()));
    assert!(argv.is_empty());
  }

  #[test]
  fn argvify_rope_freq_scale_formats_one_point_oh() {
    let knobs = TypedKnobs {
      rope_freq_scale: Some(KnobValue::Set(1.0)),
      ..TypedKnobs::default()
    };
    let argv = strs(&argvify(&knobs));
    assert_eq!(argv, vec!["--rope-freq-scale", "1.0"]);
  }

  #[test]
  fn compose_emits_knobs_then_extras_at_tail() {
    let mut p = base_params();
    p.knobs.n_gpu_layers = Some(KnobValue::Set(99));
    p.extras = vec!["--rope-freq-base".into(), "10000".into()];
    let argv = strs(&compose(&p, 41100));
    let ngl = argv.iter().position(|a| a == "--n-gpu-layers").unwrap();
    let rfb = argv.iter().position(|a| a == "--rope-freq-base").unwrap();
    assert!(ngl < rfb, "knobs must precede extras");
    assert_eq!(argv[rfb + 1], "10000");
  }

  #[test]
  fn compose_strips_forbidden_extras_flags_and_their_values() {
    let mut p = base_params();
    p.extras = vec![
      OsString::from("--host"),
      OsString::from("0.0.0.0"),
      OsString::from("--threads"),
      OsString::from("8"),
      OsString::from("--api-key=secret"),
      OsString::from("--ssl-key-file"),
      OsString::from("/etc/key.pem"),
    ];
    let argv = strs(&compose(&p, 41100));
    let host_count = argv.iter().filter(|a| *a == "--host").count();
    assert_eq!(host_count, 1, "only the bundled --host should remain");
    assert!(!argv.iter().any(|a| a == "0.0.0.0"));
    assert!(!argv.iter().any(|a| a.starts_with("--api-key")));
    assert!(!argv.iter().any(|a| a == "secret"));
    assert!(!argv.iter().any(|a| a == "--ssl-key-file"));
    assert!(!argv.iter().any(|a| a == "/etc/key.pem"));
    let t = argv.iter().position(|a| a == "--threads").unwrap();
    assert_eq!(argv[t + 1], "8");
  }

  #[test]
  fn compose_emits_extras_overlap_after_knob_so_last_wins() {
    let mut p = base_params();
    p.knobs.n_gpu_layers = Some(KnobValue::Set(99));
    p.extras = vec!["--n-gpu-layers".into(), "7".into()];
    let argv = strs(&compose(&p, 41100));
    let positions: Vec<usize> = argv
      .iter()
      .enumerate()
      .filter(|(_, a)| *a == "--n-gpu-layers")
      .map(|(i, _)| i)
      .collect();
    assert_eq!(positions.len(), 2, "both knob and extras occurrence kept");
    let last = *positions.last().unwrap();
    assert_eq!(argv[last + 1], "7", "extras occurrence is later in argv");
  }

  #[test]
  fn allocated_port_appears_after_port_flag() {
    let p = base_params();
    let argv = strs(&compose(&p, 41200));
    let i = argv.iter().position(|a| a == "--port").unwrap();
    assert_eq!(argv[i + 1], "41200");
  }

  #[test]
  fn forbidden_in_extras_flags_loopback_bypass_attempts() {
    let extras = vec![
      OsString::from("--host"),
      OsString::from("0.0.0.0"),
      OsString::from("--LISTEN=0.0.0.0:8080"),
      OsString::from("--threads"),
      OsString::from("8"),
      OsString::from("--api-key"),
      OsString::from("secret"),
      OsString::from("--ssl-key-file"),
      OsString::from("/etc/key.pem"),
    ];
    let banned = forbidden_in_extras(&extras);
    assert!(banned.iter().any(|s| s == "--host"));
    assert!(banned.iter().any(|s| s == "--LISTEN=0.0.0.0:8080"));
    assert!(banned.iter().any(|s| s == "--api-key"));
    assert!(banned.iter().any(|s| s == "--ssl-key-file"));
    assert!(!banned.iter().any(|s| s == "--threads"));
  }

  #[test]
  fn forbidden_in_extras_redacts_secret_values_in_equals_form() {
    let extras = vec![
      OsString::from("--api-key=supersecret"),
      OsString::from("--ssl-key-file=/etc/key.pem"),
      OsString::from("--host=0.0.0.0"),
    ];
    let banned = forbidden_in_extras(&extras);
    let joined = banned.join(" ");
    assert!(
      !joined.contains("supersecret"),
      "api-key value leaked into banned list: {joined}"
    );
    assert!(
      !joined.contains("/etc/key.pem"),
      "ssl path leaked into banned list: {joined}"
    );
    assert!(banned.iter().any(|s| s == "--api-key=<value-redacted>"));
    assert!(banned
      .iter()
      .any(|s| s == "--ssl-key-file=<value-redacted>"));
    // Non-secret forbidden flags (e.g. --host) keep their value — useful
    // diagnostic and not sensitive.
    assert!(banned.iter().any(|s| s == "--host=0.0.0.0"));
  }

  #[test]
  fn redact_for_display_hides_secret_values_space_form() {
    let extras = vec![
      OsString::from("--api-key"),
      OsString::from("supersecret"),
      OsString::from("--threads"),
      OsString::from("8"),
    ];
    let s = redact_for_display(&extras);
    assert!(!s.contains("supersecret"), "secret leaked: {s}");
    assert!(s.contains("--api-key <value-redacted>"));
    assert!(s.contains("--threads 8"));
  }

  #[test]
  fn redact_for_display_hides_secret_values_equals_form() {
    let extras = vec![OsString::from("--api-key=topsecret")];
    let s = redact_for_display(&extras);
    assert!(!s.contains("topsecret"));
    assert!(s.contains("--api-key=<value-redacted>"));
  }

  #[test]
  fn redact_for_display_handles_ssl_prefix() {
    let extras = vec![
      OsString::from("--ssl-key-file"),
      OsString::from("/etc/k.pem"),
    ];
    let s = redact_for_display(&extras);
    assert!(!s.contains("/etc/k.pem"));
    assert!(s.contains("--ssl-key-file <value-redacted>"));
  }

  #[test]
  fn resolve_layered_first_some_wins_per_field() {
    let _lock = crate::cli::test_lock::serialize();
    let upper = TypedKnobs {
      threads: Some(KnobValue::Set(8)),
      ..TypedKnobs::default()
    };
    let lower = TypedKnobs {
      n_gpu_layers: Some(KnobValue::Set(99)),
      threads: Some(KnobValue::Set(4)),
      ..TypedKnobs::default()
    };
    let r = resolve_layered(&[
      (LayerLabel::LastUsed, &upper),
      (LayerLabel::ArchDefault, &lower),
    ]);
    assert_eq!(
      r.knobs.threads,
      Some(KnobValue::Set(8)),
      "upper layer wins on overlap"
    );
    assert_eq!(
      r.knobs.n_gpu_layers,
      Some(KnobValue::Set(99)),
      "lower fills the unset"
    );
    assert_eq!(
      r.sources.get(&KnobField::Threads),
      Some(&LayerLabel::LastUsed)
    );
    assert_eq!(
      r.sources.get(&KnobField::NGpuLayers),
      Some(&LayerLabel::ArchDefault)
    );
    assert_eq!(
      r.sources.get(&KnobField::FlashAttn),
      Some(&LayerLabel::ServerDefault),
      "knob fields no layer filled fall through to ServerDefault"
    );
    assert_eq!(
      r.sources.get(&KnobField::Ctx),
      Some(&LayerLabel::ModelDefault),
      "ctx falls through to ModelDefault (read from GGUF when omitted)"
    );
    assert_eq!(
      r.sources.get(&KnobField::Reasoning),
      Some(&LayerLabel::ModelDefault),
      "reasoning falls through to ModelDefault (chat template decides)"
    );
  }

  #[test]
  fn seed_layerless_auto_seeds_unfilled_and_preserves_layer_values() {
    let _lock = crate::cli::test_lock::serialize();
    // user pinned `threads`, arch filled `n_gpu_layers`; `keep` is
    // layer-less.
    let user = TypedKnobs {
      threads: Some(KnobValue::Set(8)),
      ..TypedKnobs::default()
    };
    let arch = TypedKnobs {
      n_gpu_layers: Some(KnobValue::Set(99)),
      ..TypedKnobs::default()
    };
    let mut r = resolve_layered(&[(LayerLabel::User, &user), (LayerLabel::ArchDefault, &arch)]);
    seed_layerless(&mut r, DefaultLaunchMode::Auto);
    // Matrix row "sets explicit value": untouched.
    assert_eq!(r.knobs.threads, Some(KnobValue::Set(8)));
    // Matrix row "touches nothing, a layer has a value": remembered
    // value wins, not seeded to Auto.
    assert_eq!(r.knobs.n_gpu_layers, Some(KnobValue::Set(99)));
    // Layer-less + fit-governed → seeded Auto.
    assert_eq!(r.knobs.ctx, Some(KnobValue::Auto));
    assert_eq!(r.knobs.tensor_split, Some(KnobValue::Auto));
    // Layer-less but NOT fit-governed (`keep`) → left Inherited, since
    // Auto there is a no-op fit can't act on.
    assert_eq!(r.knobs.keep, None);
  }

  #[test]
  fn seed_layerless_inherited_leaves_unfilled_unset() {
    let _lock = crate::cli::test_lock::serialize();
    let empty = TypedKnobs::default();
    let mut r = resolve_layered(&[(LayerLabel::User, &empty)]);
    // Matrix row "touches nothing, no layer, mode = inherited":
    // server-default fallback, no fit-state.
    seed_layerless(&mut r, DefaultLaunchMode::Inherited);
    assert_eq!(r.knobs.keep, None);
    assert_eq!(r.knobs.ctx, None);
    assert_eq!(
      r.sources.get(&KnobField::Keep),
      Some(&LayerLabel::ServerDefault)
    );
  }

  #[test]
  fn seed_layerless_preserves_user_cycled_auto() {
    let _lock = crate::cli::test_lock::serialize();
    // Matrix row "cycles to Auto": the user's explicit Auto is a real
    // layer value, kept distinct from a seeded Auto by its source chip.
    let user = TypedKnobs {
      n_gpu_layers: Some(KnobValue::Auto),
      ..TypedKnobs::default()
    };
    let mut r = resolve_layered(&[(LayerLabel::User, &user)]);
    seed_layerless(&mut r, DefaultLaunchMode::Auto);
    assert_eq!(r.knobs.n_gpu_layers, Some(KnobValue::Auto));
    assert_eq!(
      r.sources.get(&KnobField::NGpuLayers),
      Some(&LayerLabel::User),
      "user-cycled Auto reports a User origin, not a seeded default"
    );
  }

  #[test]
  fn resolve_layered_walks_full_precedence_chain() {
    let _lock = crate::cli::test_lock::serialize();
    // R106: preset > last_used > yaml-arch > built-in. Same field
    // contributed by every layer — the highest precedence wins.
    let preset = TypedKnobs {
      threads: Some(KnobValue::Set(1)),
      ..TypedKnobs::default()
    };
    let last = TypedKnobs {
      threads: Some(KnobValue::Set(2)),
      ..TypedKnobs::default()
    };
    let yaml = TypedKnobs {
      threads: Some(KnobValue::Set(3)),
      ..TypedKnobs::default()
    };
    let builtin = TypedKnobs {
      threads: Some(KnobValue::Set(4)),
      ..TypedKnobs::default()
    };
    let r = resolve_layered(&[
      (LayerLabel::User, &preset),
      (LayerLabel::LastUsed, &last),
      (LayerLabel::ArchDefault, &yaml),
      (LayerLabel::ArchDefault, &builtin),
    ]);
    assert_eq!(r.knobs.threads, Some(KnobValue::Set(1)));
    assert_eq!(r.sources.get(&KnobField::Threads), Some(&LayerLabel::User));
  }

  #[test]
  fn resolve_layered_yaml_and_builtin_both_report_arch_default() {
    let _lock = crate::cli::test_lock::serialize();
    // Yaml and the compiled-in arch table share the `ArchDefault`
    // chip — only their per-field precedence differs.
    let yaml = TypedKnobs {
      threads: Some(KnobValue::Set(8)),
      ..TypedKnobs::default()
    };
    let builtin = TypedKnobs {
      n_gpu_layers: Some(KnobValue::Set(99)),
      ..TypedKnobs::default()
    };
    let r = resolve_layered(&[
      (LayerLabel::ArchDefault, &yaml),
      (LayerLabel::ArchDefault, &builtin),
    ]);
    assert_eq!(
      r.sources.get(&KnobField::Threads),
      Some(&LayerLabel::ArchDefault)
    );
    assert_eq!(
      r.sources.get(&KnobField::NGpuLayers),
      Some(&LayerLabel::ArchDefault)
    );
  }

  #[test]
  fn resolve_with_disable_defaults_drops_non_user_layers() {
    // Bench-disable: only the User-labeled layer's knobs survive.
    // LastUsed and ArchDefault contributions are dropped — even
    // fields the user didn't set fall through to fallback_label
    // (ServerDefault / ModelDefault) rather than inheriting.
    let user = TypedKnobs {
      n_gpu_layers: Some(KnobValue::Set(99)),
      ctx: Some(KnobValue::Set(4096)),
      ..TypedKnobs::default()
    };
    let last = TypedKnobs {
      threads: Some(KnobValue::Set(8)),
      flash_attn: Some(KnobValue::Set(true)),
      ..TypedKnobs::default()
    };
    let arch = TypedKnobs {
      batch_size: Some(KnobValue::Set(2048)),
      ubatch_size: Some(KnobValue::Set(512)),
      ..TypedKnobs::default()
    };
    let r = resolve_layered_with_disable_defaults(
      &[
        (LayerLabel::User, &user),
        (LayerLabel::LastUsed, &last),
        (LayerLabel::ArchDefault, &arch),
      ],
      true,
    );
    assert_eq!(r.knobs.n_gpu_layers, Some(KnobValue::Set(99)));
    assert_eq!(r.knobs.ctx, Some(KnobValue::Set(4096)));
    assert_eq!(
      r.knobs.threads, None,
      "last_used.threads must NOT inherit when bench-disable is on"
    );
    assert_eq!(
      r.knobs.flash_attn, None,
      "last_used.flash_attn must NOT inherit when bench-disable is on"
    );
    assert_eq!(
      r.knobs.batch_size, None,
      "arch_default.batch_size must NOT inherit when bench-disable is on"
    );
    assert_eq!(
      r.sources.get(&KnobField::Threads),
      Some(&LayerLabel::ServerDefault),
      "skipped knob falls through to ServerDefault"
    );
    assert_eq!(
      r.sources.get(&KnobField::NGpuLayers),
      Some(&LayerLabel::User),
      "user knob still labeled User"
    );
  }

  #[test]
  fn resolve_with_disable_defaults_off_preserves_full_chain() {
    // Bench-disable off: identical to plain resolve_layered. Verifies
    // the new branch doesn't accidentally alter the default path.
    let user = TypedKnobs {
      n_gpu_layers: Some(KnobValue::Set(99)),
      ..TypedKnobs::default()
    };
    let last = TypedKnobs {
      threads: Some(KnobValue::Set(8)),
      ..TypedKnobs::default()
    };
    let arch = TypedKnobs {
      batch_size: Some(KnobValue::Set(2048)),
      ..TypedKnobs::default()
    };
    let layers = [
      (LayerLabel::User, &user),
      (LayerLabel::LastUsed, &last),
      (LayerLabel::ArchDefault, &arch),
    ];
    let with_flag = resolve_layered_with_disable_defaults(&layers, false);
    let baseline = resolve_layered_inner(&layers);
    assert_eq!(with_flag.knobs, baseline.knobs);
    assert_eq!(with_flag.sources, baseline.sources);
    // Sanity: full chain inherits threads + batch_size.
    assert_eq!(with_flag.knobs.threads, Some(KnobValue::Set(8)));
    assert_eq!(with_flag.knobs.batch_size, Some(KnobValue::Set(2048)));
  }

  #[test]
  fn resolve_with_disable_defaults_and_no_user_layer_yields_empty_knobs() {
    // Edge: bench-disable but caller passed no User layer (only
    // last_used + arch defaults). Result is empty knobs with every
    // field at its fallback_label — never a stale arch-default leak.
    let last = TypedKnobs {
      threads: Some(KnobValue::Set(8)),
      ..TypedKnobs::default()
    };
    let arch = TypedKnobs {
      n_gpu_layers: Some(KnobValue::Set(99)),
      ..TypedKnobs::default()
    };
    let r = resolve_layered_with_disable_defaults(
      &[
        (LayerLabel::LastUsed, &last),
        (LayerLabel::ArchDefault, &arch),
      ],
      true,
    );
    assert_eq!(r.knobs, TypedKnobs::default());
    assert_eq!(
      r.sources.get(&KnobField::Threads),
      Some(&LayerLabel::ServerDefault)
    );
    assert_eq!(
      r.sources.get(&KnobField::NGpuLayers),
      Some(&LayerLabel::ServerDefault)
    );
  }

  #[test]
  fn bench_disable_defaults_env_var_is_strict_one() {
    // Mirrors the LLAMASTASH_ASSUME_NON_TTY contract: only "1" is on.
    // Test via the env var directly with set/restore. Holds the shared
    // `cli::test_lock` so the sibling `resolve_layered_*` tests (which
    // also grab the lock) can't observe our temporary "1" and collapse
    // to user-only layers mid-assertion.
    let _lock = crate::cli::test_lock::serialize();
    let saved = std::env::var_os("LLAMASTASH_BENCH_DISABLE_DEFAULTS");
    let restore = || match &saved {
      Some(v) => std::env::set_var("LLAMASTASH_BENCH_DISABLE_DEFAULTS", v),
      None => std::env::remove_var("LLAMASTASH_BENCH_DISABLE_DEFAULTS"),
    };

    std::env::remove_var("LLAMASTASH_BENCH_DISABLE_DEFAULTS");
    assert!(!bench_disable_defaults_from_env(), "unset → false");

    std::env::set_var("LLAMASTASH_BENCH_DISABLE_DEFAULTS", "1");
    assert!(bench_disable_defaults_from_env(), "\"1\" → true");

    for v in ["0", "true", "yes", "TRUE", ""] {
      std::env::set_var("LLAMASTASH_BENCH_DISABLE_DEFAULTS", v);
      assert!(
        !bench_disable_defaults_from_env(),
        "{v:?} must be treated as off (strict-\"1\" contract)"
      );
    }

    restore();
  }

  #[test]
  fn compose_emits_mmproj_flag_when_path_set() {
    let mut p = base_params();
    p.mmproj_path = Some(PathBuf::from("/m/mmproj-model.gguf"));
    let argv = strs(&compose(&p, 41100));
    let i = argv.iter().position(|a| a == "--mmproj").unwrap();
    assert_eq!(argv[i + 1], "/m/mmproj-model.gguf");
  }

  #[test]
  fn compose_omits_mmproj_flag_when_path_not_set() {
    let p = base_params();
    let argv = strs(&compose(&p, 41100));
    assert!(!argv.iter().any(|a| a == "--mmproj"));
  }

  #[test]
  fn launch_params_serde_round_trip() {
    let mut p = base_params();
    p.knobs.n_gpu_layers = Some(KnobValue::Set(99));
    p.extras = vec!["--rope-freq-base".into(), "10000".into()];
    let json = serde_json::to_string(&p).unwrap();
    let back: LaunchParams = serde_json::from_str(&json).unwrap();
    assert_eq!(back, p);
  }

  // ---- Device selector tests ----

  /// Collect every `--device` value present in the argv.
  fn device_values(argv: &[String]) -> Vec<&str> {
    argv
      .iter()
      .enumerate()
      .filter(|(_, a)| *a == "--device")
      .flat_map(|(i, _)| argv.get(i + 1).map(|v| v.as_str()))
      .collect()
  }

  #[test]
  fn compose_emits_selector_verbatim_exactly_once() {
    // `knobs.device` holds a real llama-server selector now. It must be
    // passed through unchanged and appear exactly once — a duplicate or
    // a mangled value (`0:0`) makes llama-server bail with
    // `invalid device`.
    for sel in ["Vulkan0", "Vulkan1", "CUDA0", "ROCm0"] {
      let mut p = base_params();
      p.knobs.device = Some(KnobValue::Set(sel.into()));
      let argv = strs(&compose(&p, 41100));
      let vals = device_values(&argv);
      assert_eq!(
        vals,
        vec![sel],
        "selector {sel} must be the only --device value"
      );
    }
  }

  #[test]
  fn compose_skips_device_when_none() {
    let p = base_params();
    assert!(p.knobs.device.is_none());
    let argv = strs(&compose(&p, 41100));
    assert!(!argv.iter().any(|a| *a == "--device"));
  }

  #[test]
  fn compose_skips_device_when_empty_string() {
    // Empty selector means "auto-select" — no flag emitted.
    let mut p = base_params();
    p.knobs.device = Some(KnobValue::Set(String::new()));
    let argv = strs(&compose(&p, 41100));
    assert!(!argv.iter().any(|a| *a == "--device"));
  }

  #[test]
  fn argvify_never_emits_device() {
    // The selector belongs to compose, not argvify — otherwise it would
    // be emitted twice.
    let knobs = TypedKnobs {
      device: Some(KnobValue::Set("Vulkan1".into())),
      ..TypedKnobs::default()
    };
    let argv = strs(&argvify(&knobs));
    assert!(!argv.iter().any(|a| a == "--device"));
  }
}
