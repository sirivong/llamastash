//! `llamastash start <model> -- <flags>` tail-args parser.
//!
//! Walks tokens left-to-right; flags recognised by
//! [`crate::launch::flag_aliases::recognise`] land on typed knobs,
//! everything else routes to `extras`. Typed-knob type/range errors
//! return `USAGE` (64); unknown flags route silently.

use std::ffi::OsString;

use crate::cli::exit_codes::{CliExit, USAGE};
use crate::config::{KnobValue, TypedKnobs};
use crate::launch::flag_aliases::KnobField;
use crate::launch::flag_aliases::{recognise, ValueKind, KV_CACHE_TYPES, SPLIT_MODES};

/// Walk `tokens` and split into (TypedKnobs, extras). Last-occurrence
/// wins for repeated knob flags.
pub fn parse_tail_args(tokens: &[OsString]) -> Result<(TypedKnobs, Vec<OsString>), CliExit> {
  let mut knobs = TypedKnobs::default();
  let mut extras: Vec<OsString> = Vec::new();
  let mut iter = tokens.iter().peekable();
  while let Some(tok) = iter.next() {
    let lossy = tok.to_string_lossy().into_owned();
    let recognised = recognise(&lossy).map(|r| {
      // Detach the lifetime from `lossy` by copying the inline value
      // into an owned `String`. Borrow checker won't let us keep
      // `Recognised<'_>` alive across the `consume_value` call below.
      (r.field, r.kind, r.inline_value.map(|s| s.to_string()))
    });
    match recognised {
      Some((field, kind, inline)) => {
        let value = match kind {
          // Booleans default to `Some(true)` for a bare flag
          // (`--flash-attn`). The equals-form (`--flash-attn=false`)
          // is honoured so a user override actually disables a knob
          // an inherited layer set to `true`. Space-form is consumed
          // only when the next token is a recognised on/off spelling
          // — modern llama-server's `--flash-attn` now requires
          // `on|off|auto`, so we mirror that. Anything else stays
          // unconsumed and routes through extras like before.
          ValueKind::Bool => {
            if let Some(v) = inline.clone() {
              Some(v)
            } else {
              match iter
                .peek()
                .map(|t| t.to_string_lossy().to_ascii_lowercase())
              {
                Some(p) if is_bool_value_token(&p) || p == "auto" => {
                  iter.next();
                  Some(p)
                }
                _ => None,
              }
            }
          }
          _ => Some(consume_value(&lossy, inline.as_deref(), &mut iter)?),
        };
        apply_knob(&mut knobs, field, kind, value.as_deref(), &lossy)?;
      }
      None => extras.push(tok.clone()),
    }
  }
  Ok((knobs, extras))
}

/// `parse_bool`-spellings that may follow a bool flag in space form,
/// matching the `on|off|true|false|...` spellings `parse_bool` accepts.
/// The literal `auto` is consumed too (handled at the call site, not
/// here) — it sets the knob's [`KnobValue::Auto`] state rather than a
/// boolean. This fixes the prior `--flash-attn auto` bug where `auto`
/// was left as a dangling positional in extras, producing broken argv.
fn is_bool_value_token(s: &str) -> bool {
  matches!(
    s,
    "on" | "off" | "true" | "false" | "1" | "0" | "yes" | "no"
  )
}

fn consume_value<'a, I>(
  flag: &str,
  inline: Option<&str>,
  iter: &mut std::iter::Peekable<I>,
) -> Result<String, CliExit>
where
  I: Iterator<Item = &'a OsString>,
{
  if let Some(v) = inline {
    return Ok(v.to_string());
  }
  let next = iter.next().ok_or_else(|| {
    CliExit::new(
      USAGE,
      format!("{flag}: missing value (expected an argument)"),
    )
  })?;
  Ok(next.to_string_lossy().into_owned())
}

fn apply_knob(
  knobs: &mut TypedKnobs,
  field: KnobField,
  kind: ValueKind,
  value: Option<&str>,
  flag: &str,
) -> Result<(), CliExit> {
  // The `auto` literal sets the knob's Auto state on *any* knob,
  // regardless of value kind — `--n-gpu-layers auto`, `--ctx auto`,
  // `--flash-attn auto` all delegate to `--fit`. For the string knobs
  // where `auto` is also a legal upstream value (split_mode / device /
  // cache_type_* / tensor_split) the knob-state meaning wins; to pass a
  // literal `auto` to llama-server, use the `--` extras tail.
  if value.is_some_and(|v| v.eq_ignore_ascii_case("auto")) {
    crate::launch::params::set_field_auto(knobs, field);
    return Ok(());
  }
  match (field, kind) {
    (KnobField::Ctx, ValueKind::U32) => knobs.ctx = Some(KnobValue::Set(parse_u32(flag, value)?)),
    (KnobField::Reasoning, ValueKind::Bool) => {
      knobs.reasoning = Some(KnobValue::Set(parse_bool(flag, value)?))
    }
    (KnobField::NGpuLayers, ValueKind::U32) => {
      knobs.n_gpu_layers = Some(KnobValue::Set(parse_u32(flag, value)?))
    }
    (KnobField::NCpuMoe, ValueKind::U32) => {
      knobs.n_cpu_moe = Some(KnobValue::Set(parse_u32(flag, value)?))
    }
    (KnobField::Threads, ValueKind::U32) => {
      knobs.threads = Some(KnobValue::Set(parse_u32(flag, value)?))
    }
    (KnobField::Parallel, ValueKind::U32) => {
      knobs.parallel = Some(KnobValue::Set(parse_u32(flag, value)?))
    }
    (KnobField::BatchSize, ValueKind::U32) => {
      knobs.batch_size = Some(KnobValue::Set(parse_u32(flag, value)?))
    }
    (KnobField::UbatchSize, ValueKind::U32) => {
      knobs.ubatch_size = Some(KnobValue::Set(parse_u32(flag, value)?))
    }
    (KnobField::Keep, ValueKind::U32) => knobs.keep = Some(KnobValue::Set(parse_u32(flag, value)?)),
    (KnobField::RopeFreqScale, ValueKind::F32) => {
      knobs.rope_freq_scale = Some(KnobValue::Set(parse_f32(flag, value)?))
    }
    (KnobField::CacheTypeK, ValueKind::KvCacheType) => {
      knobs.cache_type_k = Some(KnobValue::Set(parse_kv_cache(flag, value)?))
    }
    (KnobField::CacheTypeV, ValueKind::KvCacheType) => {
      knobs.cache_type_v = Some(KnobValue::Set(parse_kv_cache(flag, value)?))
    }
    (KnobField::FlashAttn, ValueKind::Bool) => {
      knobs.flash_attn = Some(KnobValue::Set(parse_bool(flag, value)?))
    }
    (KnobField::Mlock, ValueKind::Bool) => {
      knobs.mlock = Some(KnobValue::Set(parse_bool(flag, value)?))
    }
    (KnobField::NoMmap, ValueKind::Bool) => {
      knobs.no_mmap = Some(KnobValue::Set(parse_bool(flag, value)?))
    }
    (KnobField::Device, ValueKind::Str) => {
      knobs.device = value.map(|s| KnobValue::Set(s.to_string()));
    }
    (KnobField::TensorSplit, ValueKind::Str) => {
      knobs.tensor_split = Some(KnobValue::Set(parse_tensor_split(flag, value)?))
    }
    (KnobField::MainGpu, ValueKind::U32) => {
      knobs.main_gpu = Some(KnobValue::Set(parse_u32(flag, value)?))
    }
    (KnobField::SplitMode, ValueKind::SplitMode) => {
      knobs.split_mode = Some(KnobValue::Set(parse_split_mode(flag, value)?))
    }
    _ => {
      // Drift guard: the spec/field tables disagreed. The
      // `apply_knob_handles_every_spec_in_the_alias_table` test
      // catches this at test time so it should never fire in
      // production; treat as USAGE in the unlikely runtime case.
      return Err(CliExit::new(
        USAGE,
        format!("{flag}: internal type mismatch"),
      ));
    }
  }
  Ok(())
}

/// Parse a boolean value for a `ValueKind::Bool` knob.
///
/// - `None` (bare flag `--flash-attn`) → `true`.
/// - `Some("true" | "1" | "on" | "yes")` → `true`.
/// - `Some("false" | "0" | "off" | "no")` → `false`.
/// - Anything else → `USAGE` with the offending token quoted.
///
/// Case-insensitive on the value so `--flash-attn=FALSE` works too.
fn parse_bool(flag: &str, value: Option<&str>) -> Result<bool, CliExit> {
  let Some(v) = value else {
    return Ok(true);
  };
  match v.to_ascii_lowercase().as_str() {
    "true" | "1" | "on" | "yes" => Ok(true),
    "false" | "0" | "off" | "no" => Ok(false),
    _ => Err(CliExit::new(
      USAGE,
      format!("{flag}: expected true/false (or 1/0, on/off, yes/no), got {v:?}"),
    )),
  }
}

fn parse_u32(flag: &str, value: Option<&str>) -> Result<u32, CliExit> {
  let v = value.ok_or_else(|| CliExit::new(USAGE, format!("{flag}: expected u32")))?;
  v.parse::<u32>()
    .map_err(|_| CliExit::new(USAGE, format!("{flag}: expected u32, got {v:?}")))
}

fn parse_f32(flag: &str, value: Option<&str>) -> Result<f32, CliExit> {
  let v = value.ok_or_else(|| CliExit::new(USAGE, format!("{flag}: expected float")))?;
  v.parse::<f32>()
    .map_err(|_| CliExit::new(USAGE, format!("{flag}: expected float, got {v:?}")))
}

/// Parse `--split-mode` against the allowed set (`none|layer|row`).
fn parse_split_mode(flag: &str, value: Option<&str>) -> Result<String, CliExit> {
  let v = value.ok_or_else(|| {
    CliExit::new(
      USAGE,
      format!("{flag}: expected one of {}", SPLIT_MODES.join(", ")),
    )
  })?;
  if SPLIT_MODES.contains(&v) {
    Ok(v.to_string())
  } else {
    Err(CliExit::new(
      USAGE,
      format!(
        "{flag}: expected one of {}, got {v:?}",
        SPLIT_MODES.join(", ")
      ),
    ))
  }
}

/// Parse `--tensor-split` — a comma-separated list of proportions
/// (`"3,1"`, `"0.6,0.4"`). Validated to comma-separated numbers so a
/// typo fails loudly at the CLI instead of inside llama-server, then
/// stored verbatim for the child.
fn parse_tensor_split(flag: &str, value: Option<&str>) -> Result<String, CliExit> {
  let v = value
    .map(str::trim)
    .filter(|s| !s.is_empty())
    .ok_or_else(|| {
      CliExit::new(
        USAGE,
        format!("{flag}: expected comma-separated numbers (e.g. 3,1)"),
      )
    })?;
  let all_numeric = v
    .split(',')
    .all(|part| !part.trim().is_empty() && part.trim().parse::<f32>().is_ok());
  if all_numeric {
    Ok(v.to_string())
  } else {
    Err(CliExit::new(
      USAGE,
      format!("{flag}: expected comma-separated numbers (e.g. 3,1), got {v:?}"),
    ))
  }
}

fn parse_kv_cache(flag: &str, value: Option<&str>) -> Result<String, CliExit> {
  let v = value.ok_or_else(|| {
    CliExit::new(
      USAGE,
      format!(
        "{flag}: expected a cache type (e.g. {})",
        KV_CACHE_TYPES.join(", ")
      ),
    )
  })?;
  if KV_CACHE_TYPES.contains(&v) || is_custom_kv_cache_type(v) {
    Ok(v.to_string())
  } else {
    Err(CliExit::new(
      USAGE,
      format!(
        "{flag}: {v:?} is not a recognized cache type; known types: {}. \
         Custom types from a modified llama-server build must start with a \
         letter and contain only letters, digits, and underscores \
         (e.g. fp4, turbo_quant).",
        KV_CACHE_TYPES.join(", ")
      ),
    ))
  }
}

/// Returns `true` when `s` looks like a cache-type identifier a custom
/// `llama-server` build might define — starts with an ASCII letter and
/// contains only ASCII letters, digits, and underscores. This lets
/// non-standard types (e.g. `fp4`, `turbo_quant`) clear the CLI / TUI
/// gate so `llama-server` itself is the authority on whether the type is
/// actually supported, rather than llamastash rejecting it up front.
pub fn is_custom_kv_cache_type(s: &str) -> bool {
  let mut chars = s.chars();
  match chars.next() {
    Some(first) if first.is_ascii_alphabetic() => {
      chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    }
    _ => false,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::config::KnobValueOpt;

  fn osvec(args: &[&str]) -> Vec<OsString> {
    args.iter().map(|s| OsString::from(*s)).collect()
  }

  #[test]
  fn happy_path_threads_and_flash_attn() {
    let (knobs, extras) = parse_tail_args(&osvec(&["--threads", "8", "--flash-attn"])).unwrap();
    assert_eq!(knobs.threads, Some(KnobValue::Set(8)));
    assert_eq!(knobs.flash_attn, Some(KnobValue::Set(true)));
    assert!(extras.is_empty());
  }

  #[test]
  fn short_alias_ngl() {
    let (knobs, extras) = parse_tail_args(&osvec(&["-ngl", "99"])).unwrap();
    assert_eq!(knobs.n_gpu_layers, Some(KnobValue::Set(99)));
    assert!(extras.is_empty());
  }

  #[test]
  fn n_cpu_moe_parses_canonical_and_alias() {
    let (knobs, extras) = parse_tail_args(&osvec(&["--n-cpu-moe", "12"])).unwrap();
    assert_eq!(knobs.n_cpu_moe, Some(KnobValue::Set(12)));
    assert!(extras.is_empty());
    let (alias, _) = parse_tail_args(&osvec(&["-ncmoe", "8"])).unwrap();
    assert_eq!(alias.n_cpu_moe, Some(KnobValue::Set(8)));
  }

  #[test]
  fn placement_knobs_parse_canonical_and_alias() {
    let (k, extras) = parse_tail_args(&osvec(&[
      "--tensor-split",
      "3,1",
      "--main-gpu",
      "0",
      "--split-mode",
      "row",
    ]))
    .unwrap();
    assert_eq!(k.tensor_split.set_value().map(String::as_str), Some("3,1"));
    assert_eq!(k.main_gpu, Some(KnobValue::Set(0)));
    assert_eq!(k.split_mode.set_value().map(String::as_str), Some("row"));
    assert!(extras.is_empty());
    let (alias, _) =
      parse_tail_args(&osvec(&["-ts", "2,1,1", "-mg", "1", "-sm", "layer"])).unwrap();
    assert_eq!(
      alias.tensor_split.set_value().map(String::as_str),
      Some("2,1,1")
    );
    assert_eq!(alias.main_gpu, Some(KnobValue::Set(1)));
    assert_eq!(
      alias.split_mode.set_value().map(String::as_str),
      Some("layer")
    );
  }

  #[test]
  fn split_mode_validates_set() {
    let err = parse_tail_args(&osvec(&["--split-mode", "diagonal"])).unwrap_err();
    assert_eq!(err.code, USAGE);
    let msg = err.to_string();
    assert!(msg.contains("none, layer, row"), "{msg}");
  }

  #[test]
  fn tensor_split_rejects_non_numeric() {
    let err = parse_tail_args(&osvec(&["--tensor-split", "3,x"])).unwrap_err();
    assert_eq!(err.code, USAGE);
    let msg = err.to_string();
    assert!(msg.contains("--tensor-split"), "{msg}");
    // A valid ratio round-trips verbatim.
    let (k, _) = parse_tail_args(&osvec(&["--tensor-split", "0.6,0.4"])).unwrap();
    assert_eq!(
      k.tensor_split.set_value().map(String::as_str),
      Some("0.6,0.4")
    );
  }

  #[test]
  fn equals_form_parses_identically() {
    let (knobs, _) = parse_tail_args(&osvec(&["--threads=8"])).unwrap();
    assert_eq!(knobs.threads, Some(KnobValue::Set(8)));
  }

  #[test]
  fn unknown_token_routes_to_extras() {
    let (knobs, extras) = parse_tail_args(&osvec(&["--rope-freq-base", "10000"])).unwrap();
    assert_eq!(knobs, TypedKnobs::default());
    assert_eq!(
      extras,
      vec![OsString::from("--rope-freq-base"), OsString::from("10000")]
    );
  }

  #[test]
  fn typed_knob_type_error_returns_usage() {
    let err = parse_tail_args(&osvec(&["--threads", "xyz"])).unwrap_err();
    assert_eq!(err.code, USAGE);
    let msg = err.to_string();
    assert!(msg.contains("--threads"), "msg should name the flag: {msg}");
    assert!(msg.contains("xyz"), "msg should quote the bad token: {msg}");
  }

  #[test]
  fn missing_value_returns_usage() {
    let err = parse_tail_args(&osvec(&["--n-gpu-layers"])).unwrap_err();
    assert_eq!(err.code, USAGE);
    let msg = err.to_string();
    assert!(msg.contains("--n-gpu-layers"));
  }

  #[test]
  fn last_occurrence_wins() {
    let (knobs, _) = parse_tail_args(&osvec(&["--threads", "4", "--threads", "16"])).unwrap();
    assert_eq!(knobs.threads, Some(KnobValue::Set(16)));
  }

  #[test]
  fn boolean_does_not_consume_next_flag() {
    let (knobs, _) = parse_tail_args(&osvec(&["--flash-attn", "--threads", "8"])).unwrap();
    assert_eq!(knobs.flash_attn, Some(KnobValue::Set(true)));
    assert_eq!(knobs.threads, Some(KnobValue::Set(8)));
  }

  #[test]
  fn boolean_space_form_consumes_on_off_value() {
    // Modern llama-server requires `--flash-attn on|off|auto`; the
    // bench harness emits the space form, so the parser must absorb
    // the value rather than leaving it as an orphan positional.
    let (knobs_on, extras_on) = parse_tail_args(&osvec(&["--flash-attn", "on"])).unwrap();
    assert_eq!(knobs_on.flash_attn, Some(KnobValue::Set(true)));
    assert!(
      extras_on.is_empty(),
      "`on` must be consumed, not routed to extras: {extras_on:?}"
    );

    let (knobs_off, extras_off) = parse_tail_args(&osvec(&["--flash-attn", "off"])).unwrap();
    assert_eq!(knobs_off.flash_attn, Some(KnobValue::Set(false)));
    assert!(extras_off.is_empty());
  }

  #[test]
  fn flash_attn_auto_sets_auto_state_with_no_dangling_extra() {
    // Regression for the latent bug: `--flash-attn auto` previously
    // left `auto` as a dangling positional in extras *and* set
    // flash_attn=true, producing broken argv. It now consumes `auto`
    // and sets the knob's Auto state — nothing leaks to extras.
    let (knobs, extras) = parse_tail_args(&osvec(&["--flash-attn", "auto"])).unwrap();
    assert_eq!(knobs.flash_attn, Some(KnobValue::Auto));
    assert!(
      extras.is_empty(),
      "`auto` must be consumed, not left dangling: {extras:?}"
    );
  }

  #[test]
  fn auto_literal_sets_auto_on_every_knob_kind() {
    // Numeric, string, and equals-form all route `auto` to the Auto
    // state rather than parsing it as a value.
    let (k, extras) = parse_tail_args(&osvec(&[
      "--n-gpu-layers",
      "auto",
      "--threads=auto",
      "--split-mode=auto",
    ]))
    .unwrap();
    assert_eq!(k.n_gpu_layers, Some(KnobValue::Auto));
    assert_eq!(k.threads, Some(KnobValue::Auto));
    // `auto` is a legal upstream value for split_mode, but the knob
    // state wins (use extras to pass a literal `auto` to the server).
    assert_eq!(k.split_mode, Some(KnobValue::Auto));
    assert!(extras.is_empty());

    // A concrete value still parses to Set.
    let (k2, _) = parse_tail_args(&osvec(&["--n-gpu-layers", "50"])).unwrap();
    assert_eq!(k2.n_gpu_layers, Some(KnobValue::Set(50)));
  }

  #[test]
  fn bool_equals_false_sets_explicit_off() {
    // Lets users override a built-in `Some(true)` from the CLI
    // without having to round-trip through YAML or the TUI.
    let (knobs, extras) = parse_tail_args(&osvec(&["--flash-attn=false"])).unwrap();
    assert_eq!(knobs.flash_attn, Some(KnobValue::Set(false)));
    assert!(extras.is_empty());
  }

  #[test]
  fn bool_equals_true_sets_explicit_on() {
    let (knobs, _) = parse_tail_args(&osvec(&["--flash-attn=true"])).unwrap();
    assert_eq!(knobs.flash_attn, Some(KnobValue::Set(true)));
  }

  #[test]
  fn bool_accepts_alternate_truthy_falsy_spellings() {
    for spelling in ["1", "on", "yes", "TRUE", "True"] {
      let (knobs, _) = parse_tail_args(&osvec(&[&format!("--mlock={spelling}")])).unwrap();
      assert_eq!(
        knobs.mlock,
        Some(KnobValue::Set(true)),
        "{spelling:?} should parse to Some(true)"
      );
    }
    for spelling in ["0", "off", "no", "FALSE", "False"] {
      let (knobs, _) = parse_tail_args(&osvec(&[&format!("--mlock={spelling}")])).unwrap();
      assert_eq!(
        knobs.mlock,
        Some(KnobValue::Set(false)),
        "{spelling:?} should parse to Some(false)"
      );
    }
  }

  #[test]
  fn bool_rejects_garbage_value_with_usage_and_named_flag() {
    let err = parse_tail_args(&osvec(&["--flash-attn=maybe"])).unwrap_err();
    assert_eq!(err.code, USAGE);
    let msg = err.to_string();
    assert!(msg.contains("--flash-attn"), "msg must name flag: {msg}");
    assert!(msg.contains("maybe"), "msg must quote value: {msg}");
  }

  #[test]
  fn cache_type_k_validates_set() {
    // Every standard llama-server type, plus custom identifiers from
    // modified builds, parse through to the typed slot unchanged.
    for t in [
      "f32",
      "f16",
      "bf16",
      "q8_0",
      "q4_0",
      "q4_1",
      "iq4_nl",
      "q5_0",
      "q5_1",
      "fp4",
      "turbo_quant",
      "myfmt0",
    ] {
      let (parsed, _) = parse_tail_args(&osvec(&["--cache-type-k", t])).expect(t);
      assert_eq!(parsed.cache_type_k.set_value().map(String::as_str), Some(t));
    }
    // Identifiers that can't name a type (leading digit, embedded space)
    // are still rejected with a USAGE error that lists the known set.
    let err = parse_tail_args(&osvec(&["--cache-type-k", "4bad"])).unwrap_err();
    assert_eq!(err.code, USAGE);
    assert!(err.to_string().contains("f16, bf16, q8_0"), "{err}");
    assert_eq!(
      parse_tail_args(&osvec(&["--cache-type-k", "bad type"]))
        .unwrap_err()
        .code,
      USAGE
    );
  }

  #[test]
  fn is_custom_kv_cache_type_accepts_quant_ids_and_rejects_garbage() {
    // Permissive on purpose: anything that could name a quant type passes
    // so llama-server stays the authority. The gate only rejects what
    // could not be a single identifier token.
    for ok in ["fp4", "turbo_quant", "q4_0", "iq4_nl", "a", "FP8"] {
      assert!(is_custom_kv_cache_type(ok), "should accept {ok:?}");
    }
    for bad in ["", "4bad", "_lead", "has space", "dash-no"] {
      assert!(!is_custom_kv_cache_type(bad), "should reject {bad:?}");
    }
  }

  #[test]
  fn rope_freq_scale_accepts_float() {
    let (knobs, _) = parse_tail_args(&osvec(&["--rope-freq-scale", "0.5"])).unwrap();
    assert_eq!(knobs.rope_freq_scale, Some(KnobValue::Set(0.5)));
  }

  /// Drift guard: every spec in [`crate::launch::flag_aliases::knob_specs`]
  /// must have a matching arm in `apply_knob`. Without this test, adding
  /// a new knob and forgetting to extend the dispatch surfaces only as a
  /// generic "internal type mismatch" `USAGE` error at runtime.
  #[test]
  fn apply_knob_handles_every_spec_in_the_alias_table() {
    use crate::launch::flag_aliases::{knob_specs, KV_CACHE_TYPES, SPLIT_MODES};
    for spec in knob_specs() {
      let value: Option<&str> = match spec.kind {
        ValueKind::U32 => Some("1"),
        ValueKind::F32 => Some("1.0"),
        ValueKind::KvCacheType => Some(KV_CACHE_TYPES[0]),
        ValueKind::SplitMode => Some(SPLIT_MODES[0]),
        ValueKind::Bool => None,
        // `0` is a valid one-element tensor-split and a non-empty
        // device selector, so both Str knobs mutate from this sample.
        ValueKind::Str => Some("0"),
      };
      let mut knobs = TypedKnobs::default();
      apply_knob(&mut knobs, spec.field, spec.kind, value, spec.canonical).unwrap_or_else(|err| {
        panic!(
          "apply_knob lacks an arm for ({:?}, {:?}) — flag {}: {}",
          spec.field, spec.kind, spec.canonical, err
        )
      });
      assert_ne!(
        knobs,
        TypedKnobs::default(),
        "{:?} arm did not actually mutate any field",
        spec.field
      );
    }
  }

  #[test]
  fn mixed_knobs_and_extras() {
    let (knobs, extras) = parse_tail_args(&osvec(&[
      "--threads",
      "8",
      "--rope-freq-base",
      "10000",
      "-ngl",
      "99",
    ]))
    .unwrap();
    assert_eq!(knobs.threads, Some(KnobValue::Set(8)));
    assert_eq!(knobs.n_gpu_layers, Some(KnobValue::Set(99)));
    assert_eq!(
      extras,
      vec![OsString::from("--rope-freq-base"), OsString::from("10000")]
    );
  }
}
