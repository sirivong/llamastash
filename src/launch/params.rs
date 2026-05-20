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

use crate::config::TypedKnobs;
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

/// Returns the subset of `extras` flags that hit the denylist. Used
/// by IPC handlers to refuse a launch before spawn, and by `compose`
/// to defensively strip in case validation was skipped.
pub fn forbidden_in_extras(extras: &[OsString]) -> Vec<String> {
  extras
    .iter()
    .filter_map(|s| {
      let lossy = s.to_string_lossy();
      let head = lossy.split('=').next().unwrap_or(&lossy);
      if is_forbidden_head(head) {
        Some(lossy.into_owned())
      } else {
        None
      }
    })
    .collect()
}

/// Format an extras list for human display, redacting values that
/// follow secret-bearing prefixes (`--api-key`, `--ssl-*`). Used by
/// the TUI's forbidden-flag inline warning and any other surface
/// that might echo extras back to a log or terminal.
pub fn redact_for_display(extras: &[OsString]) -> String {
  let secret_prefixes: &[&str] = &["--api-key", "--ssl-"];
  let is_secret = |head: &str| {
    let lower = head.to_ascii_lowercase();
    secret_prefixes
      .iter()
      .any(|p| lower == *p || (p.ends_with('-') && lower.starts_with(p)))
  };
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

/// All launch knobs the supervisor reads. Persisted under
/// `last_params: HashMap<ModelId, LaunchParams>` in `state.json`.
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
  pub ctx: Option<u32>,
  /// Listening port. `None` leaves port allocation to the supervisor.
  pub port: Option<u16>,
  /// Reasoning bundle on/off. When `true`, supervisor appends
  /// `--jinja --reasoning-format deepseek` to the argv.
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
}

impl LaunchParams {
  pub fn new(model_path: PathBuf, mode: LaunchMode) -> Self {
    Self {
      model_path,
      mode,
      ctx: None,
      port: None,
      reasoning: false,
      knobs: TypedKnobs::default(),
      extras: Vec::new(),
    }
  }
}

/// Argv-ify the typed knob set in canonical flag order. Skips
/// `None` fields; for booleans, only emits the flag when
/// `Some(true)` (`Some(false)` is an explicit opt-out — no
/// `--no-flash-attn` form because llama-server doesn't have one).
pub fn argvify(knobs: &TypedKnobs) -> Vec<OsString> {
  let mut out: Vec<OsString> = Vec::new();
  for spec in knob_specs() {
    match spec.field {
      KnobField::NGpuLayers => push_u32(&mut out, spec.canonical, knobs.n_gpu_layers),
      KnobField::Threads => push_u32(&mut out, spec.canonical, knobs.threads),
      KnobField::CacheTypeK => push_str(&mut out, spec.canonical, knobs.cache_type_k.as_deref()),
      KnobField::CacheTypeV => push_str(&mut out, spec.canonical, knobs.cache_type_v.as_deref()),
      KnobField::Parallel => push_u32(&mut out, spec.canonical, knobs.parallel),
      KnobField::FlashAttn => push_bool(&mut out, spec.canonical, knobs.flash_attn),
      KnobField::Mlock => push_bool(&mut out, spec.canonical, knobs.mlock),
      KnobField::NoMmap => push_bool(&mut out, spec.canonical, knobs.no_mmap),
      KnobField::BatchSize => push_u32(&mut out, spec.canonical, knobs.batch_size),
      KnobField::UbatchSize => push_u32(&mut out, spec.canonical, knobs.ubatch_size),
      KnobField::RopeFreqScale => push_f32(&mut out, spec.canonical, knobs.rope_freq_scale),
      KnobField::Keep => push_u32(&mut out, spec.canonical, knobs.keep),
    }
    // `ValueKind` is the source-of-truth for emission shape; sanity
    // check that our match handled the right kind.
    debug_assert!(
      matches!(
        spec.kind,
        ValueKind::U32 | ValueKind::F32 | ValueKind::Bool | ValueKind::KvCacheType
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
/// `(built-in)`, `(model default)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LayerLabel {
  User,
  LastUsed,
  ArchDefault,
  BuiltIn,
  ModelDefault,
}

impl LayerLabel {
  /// Human-readable, single-token label rendered in the editor.
  pub fn label(self) -> &'static str {
    match self {
      LayerLabel::User => "user",
      LayerLabel::LastUsed => "last used",
      LayerLabel::ArchDefault => "arch default",
      LayerLabel::BuiltIn => "built-in",
      LayerLabel::ModelDefault => "model default",
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
/// (ArchDefault, &yaml), (BuiltIn, &table_lookup)]`; anything still
/// `None` after that walk is annotated `ModelDefault` (llama-server
/// will fall back to its own default).
pub fn resolve_layered(layers: &[(LayerLabel, &TypedKnobs)]) -> Resolved {
  let mut knobs = TypedKnobs::default();
  let mut sources: BTreeMap<KnobField, LayerLabel> = BTreeMap::new();
  for spec in knob_specs() {
    sources.insert(spec.field, LayerLabel::ModelDefault);
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

/// If `field` is `Some` on `from` and `None` on `into`, copy it.
/// Returns true when a copy happened.
fn try_inherit_field(into: &mut TypedKnobs, from: &TypedKnobs, field: KnobField) -> bool {
  match field {
    KnobField::NGpuLayers => copy_some(&mut into.n_gpu_layers, from.n_gpu_layers),
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
  }
}

fn copy_some<T: Copy>(into: &mut Option<T>, from: Option<T>) -> bool {
  if into.is_none() {
    if let Some(v) = from {
      *into = Some(v);
      return true;
    }
  }
  false
}

fn copy_some_clone(into: &mut Option<String>, from: &Option<String>) -> bool {
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
pub fn compose(params: &LaunchParams, allocated_port: u16) -> Vec<OsString> {
  let knob_argv = argvify(&params.knobs);
  let mut argv: Vec<OsString> = Vec::with_capacity(16 + knob_argv.len() + params.extras.len());
  argv.push("--host".into());
  argv.push("127.0.0.1".into());
  argv.push("--port".into());
  argv.push(allocated_port.to_string().into());
  argv.push("-m".into());
  argv.push(params.model_path.clone().into());
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
  if let Some(ctx) = params.ctx {
    argv.push("-c".into());
    argv.push(ctx.to_string().into());
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
      n_gpu_layers: Some(99),
      threads: Some(8),
      cache_type_k: Some("q8_0".into()),
      cache_type_v: Some("q8_0".into()),
      flash_attn: Some(true),
      mlock: Some(true),
      no_mmap: Some(true),
      parallel: Some(4),
      batch_size: Some(2048),
      ubatch_size: Some(512),
      rope_freq_scale: Some(1.0),
      keep: Some(128),
    };
    let argv = strs(&argvify(&knobs));
    assert_eq!(
      argv,
      vec![
        "--n-gpu-layers",
        "99",
        "--threads",
        "8",
        "--cache-type-k",
        "q8_0",
        "--cache-type-v",
        "q8_0",
        "--parallel",
        "4",
        "--flash-attn",
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
      n_gpu_layers: Some(99),
      flash_attn: Some(true),
      ..TypedKnobs::default()
    };
    let argv = strs(&argvify(&knobs));
    assert_eq!(argv, vec!["--n-gpu-layers", "99", "--flash-attn"]);
  }

  #[test]
  fn argvify_some_false_omits_boolean_flag() {
    let knobs = TypedKnobs {
      flash_attn: Some(false),
      mlock: Some(false),
      ..TypedKnobs::default()
    };
    let argv = strs(&argvify(&knobs));
    assert!(argv.is_empty(), "Some(false) bools must not emit the flag");
  }

  #[test]
  fn argvify_empty_yields_empty() {
    let argv = strs(&argvify(&TypedKnobs::default()));
    assert!(argv.is_empty());
  }

  #[test]
  fn argvify_rope_freq_scale_formats_one_point_oh() {
    let knobs = TypedKnobs {
      rope_freq_scale: Some(1.0),
      ..TypedKnobs::default()
    };
    let argv = strs(&argvify(&knobs));
    assert_eq!(argv, vec!["--rope-freq-scale", "1.0"]);
  }

  #[test]
  fn compose_emits_knobs_then_extras_at_tail() {
    let mut p = base_params();
    p.knobs.n_gpu_layers = Some(99);
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
    p.knobs.n_gpu_layers = Some(99);
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
    let upper = TypedKnobs {
      threads: Some(8),
      ..TypedKnobs::default()
    };
    let lower = TypedKnobs {
      n_gpu_layers: Some(99),
      threads: Some(4),
      ..TypedKnobs::default()
    };
    let r = resolve_layered(&[
      (LayerLabel::LastUsed, &upper),
      (LayerLabel::BuiltIn, &lower),
    ]);
    assert_eq!(r.knobs.threads, Some(8), "upper layer wins on overlap");
    assert_eq!(r.knobs.n_gpu_layers, Some(99), "lower fills the unset");
    assert_eq!(
      r.sources.get(&KnobField::Threads),
      Some(&LayerLabel::LastUsed)
    );
    assert_eq!(
      r.sources.get(&KnobField::NGpuLayers),
      Some(&LayerLabel::BuiltIn)
    );
    assert_eq!(
      r.sources.get(&KnobField::FlashAttn),
      Some(&LayerLabel::ModelDefault),
      "fields no layer filled fall through to ModelDefault"
    );
  }

  #[test]
  fn resolve_layered_walks_full_precedence_chain() {
    // R106: preset > last_used > yaml-arch > built-in. Same field
    // contributed by every layer — the highest precedence wins.
    let preset = TypedKnobs {
      threads: Some(1),
      ..TypedKnobs::default()
    };
    let last = TypedKnobs {
      threads: Some(2),
      ..TypedKnobs::default()
    };
    let yaml = TypedKnobs {
      threads: Some(3),
      ..TypedKnobs::default()
    };
    let builtin = TypedKnobs {
      threads: Some(4),
      ..TypedKnobs::default()
    };
    let r = resolve_layered(&[
      (LayerLabel::User, &preset),
      (LayerLabel::LastUsed, &last),
      (LayerLabel::ArchDefault, &yaml),
      (LayerLabel::BuiltIn, &builtin),
    ]);
    assert_eq!(r.knobs.threads, Some(1));
    assert_eq!(r.sources.get(&KnobField::Threads), Some(&LayerLabel::User));
  }

  #[test]
  fn launch_params_serde_round_trip() {
    let mut p = base_params();
    p.knobs.n_gpu_layers = Some(99);
    p.extras = vec!["--rope-freq-base".into(), "10000".into()];
    let json = serde_json::to_string(&p).unwrap();
    let back: LaunchParams = serde_json::from_str(&json).unwrap();
    assert_eq!(back, p);
  }
}
