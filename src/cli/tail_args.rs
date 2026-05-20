//! `llamastash start <model> -- <flags>` tail-args parser.
//!
//! Walks tokens left-to-right; flags recognised by
//! [`crate::launch::flag_aliases::recognise`] land on typed knobs,
//! everything else routes to `extras`. Typed-knob type/range errors
//! return `USAGE` (64); unknown flags route silently.

use std::ffi::OsString;

use crate::cli::exit_codes::{CliExit, USAGE};
use crate::config::TypedKnobs;
use crate::launch::flag_aliases::KnobField;
use crate::launch::flag_aliases::{recognise, ValueKind, KV_CACHE_TYPES};

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
          ValueKind::Bool => None,
          _ => Some(consume_value(&lossy, inline.as_deref(), &mut iter)?),
        };
        apply_knob(&mut knobs, field, kind, value.as_deref(), &lossy)?;
      }
      None => extras.push(tok.clone()),
    }
  }
  Ok((knobs, extras))
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
  match (field, kind) {
    (KnobField::NGpuLayers, ValueKind::U32) => knobs.n_gpu_layers = Some(parse_u32(flag, value)?),
    (KnobField::Threads, ValueKind::U32) => knobs.threads = Some(parse_u32(flag, value)?),
    (KnobField::Parallel, ValueKind::U32) => knobs.parallel = Some(parse_u32(flag, value)?),
    (KnobField::BatchSize, ValueKind::U32) => knobs.batch_size = Some(parse_u32(flag, value)?),
    (KnobField::UbatchSize, ValueKind::U32) => knobs.ubatch_size = Some(parse_u32(flag, value)?),
    (KnobField::Keep, ValueKind::U32) => knobs.keep = Some(parse_u32(flag, value)?),
    (KnobField::RopeFreqScale, ValueKind::F32) => {
      knobs.rope_freq_scale = Some(parse_f32(flag, value)?)
    }
    (KnobField::CacheTypeK, ValueKind::KvCacheType) => {
      knobs.cache_type_k = Some(parse_kv_cache(flag, value)?)
    }
    (KnobField::CacheTypeV, ValueKind::KvCacheType) => {
      knobs.cache_type_v = Some(parse_kv_cache(flag, value)?)
    }
    (KnobField::FlashAttn, ValueKind::Bool) => knobs.flash_attn = Some(true),
    (KnobField::Mlock, ValueKind::Bool) => knobs.mlock = Some(true),
    (KnobField::NoMmap, ValueKind::Bool) => knobs.no_mmap = Some(true),
    _ => {
      // Drift guard: the spec/field tables disagreed. Treat as USAGE.
      return Err(CliExit::new(
        USAGE,
        format!("{flag}: internal type mismatch"),
      ));
    }
  }
  Ok(())
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

fn parse_kv_cache(flag: &str, value: Option<&str>) -> Result<String, CliExit> {
  let v = value.ok_or_else(|| {
    CliExit::new(
      USAGE,
      format!("{flag}: expected one of {}", KV_CACHE_TYPES.join(", ")),
    )
  })?;
  if KV_CACHE_TYPES.contains(&v) {
    Ok(v.to_string())
  } else {
    Err(CliExit::new(
      USAGE,
      format!(
        "{flag}: expected one of {}, got {v:?}",
        KV_CACHE_TYPES.join(", ")
      ),
    ))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn osvec(args: &[&str]) -> Vec<OsString> {
    args.iter().map(|s| OsString::from(*s)).collect()
  }

  #[test]
  fn happy_path_threads_and_flash_attn() {
    let (knobs, extras) = parse_tail_args(&osvec(&["--threads", "8", "--flash-attn"])).unwrap();
    assert_eq!(knobs.threads, Some(8));
    assert_eq!(knobs.flash_attn, Some(true));
    assert!(extras.is_empty());
  }

  #[test]
  fn short_alias_ngl() {
    let (knobs, extras) = parse_tail_args(&osvec(&["-ngl", "99"])).unwrap();
    assert_eq!(knobs.n_gpu_layers, Some(99));
    assert!(extras.is_empty());
  }

  #[test]
  fn equals_form_parses_identically() {
    let (knobs, _) = parse_tail_args(&osvec(&["--threads=8"])).unwrap();
    assert_eq!(knobs.threads, Some(8));
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
    assert_eq!(knobs.threads, Some(16));
  }

  #[test]
  fn boolean_does_not_consume_next_flag() {
    let (knobs, _) = parse_tail_args(&osvec(&["--flash-attn", "--threads", "8"])).unwrap();
    assert_eq!(knobs.flash_attn, Some(true));
    assert_eq!(knobs.threads, Some(8));
  }

  #[test]
  fn cache_type_k_validates_set() {
    let (knobs, _) = parse_tail_args(&osvec(&["--cache-type-k", "q8_0"])).unwrap();
    assert_eq!(knobs.cache_type_k.as_deref(), Some("q8_0"));
    let err = parse_tail_args(&osvec(&["--cache-type-k", "q9_0"])).unwrap_err();
    assert_eq!(err.code, USAGE);
    let msg = err.to_string();
    assert!(msg.contains("f16, q8_0, q4_0"), "{msg}");
  }

  #[test]
  fn rope_freq_scale_accepts_float() {
    let (knobs, _) = parse_tail_args(&osvec(&["--rope-freq-scale", "0.5"])).unwrap();
    assert_eq!(knobs.rope_freq_scale, Some(0.5));
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
    assert_eq!(knobs.threads, Some(8));
    assert_eq!(knobs.n_gpu_layers, Some(99));
    assert_eq!(
      extras,
      vec![OsString::from("--rope-freq-base"), OsString::from("10000")]
    );
  }
}
