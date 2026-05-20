//! Shared flag-alias recognition for the typed-knob surface.
//!
//! Three call sites need to recognise the same `--n-gpu-layers` /
//! `-ngl` / `--n-gpu-layers=N` alias families: the `compose` argv
//! emitter (so it emits canonical flag names), the CLI tail-args
//! parser (so it routes recognised flags into typed slots), and the
//! TUI extras-row inline edit. Centralising the table here keeps
//! those three in lock-step.

/// One typed knob the editor surfaces. Keep in sync with
/// `TypedKnobs` in `crate::config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KnobField {
  NGpuLayers,
  Threads,
  CacheTypeK,
  CacheTypeV,
  FlashAttn,
  Mlock,
  NoMmap,
  Parallel,
  BatchSize,
  UbatchSize,
  RopeFreqScale,
  Keep,
}

/// What the parser expects after the flag head. Bool consumes no
/// value; everything else takes one (either the next token or the
/// `=value` suffix).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
  U32,
  F32,
  Bool,
  /// `cache_type_k` / `cache_type_v` allowed set.
  KvCacheType,
}

/// One row in the alias table.
pub struct KnobSpec {
  pub field: KnobField,
  pub canonical: &'static str,
  pub aliases: &'static [&'static str],
  pub kind: ValueKind,
}

/// Allowed values for `cache_type_k` / `cache_type_v` (matches
/// llama-server's documented k/v cache quant types).
pub const KV_CACHE_TYPES: &[&str] = &["f16", "q8_0", "q4_0"];

/// Canonical emission order. Pinned by the plan's Risks & Dependencies
/// table to keep argv diffs readable across releases.
const SPECS: &[KnobSpec] = &[
  KnobSpec {
    field: KnobField::NGpuLayers,
    canonical: "--n-gpu-layers",
    aliases: &["-ngl"],
    kind: ValueKind::U32,
  },
  KnobSpec {
    field: KnobField::Threads,
    canonical: "--threads",
    aliases: &["-t"],
    kind: ValueKind::U32,
  },
  KnobSpec {
    field: KnobField::CacheTypeK,
    canonical: "--cache-type-k",
    aliases: &["-ctk"],
    kind: ValueKind::KvCacheType,
  },
  KnobSpec {
    field: KnobField::CacheTypeV,
    canonical: "--cache-type-v",
    aliases: &["-ctv"],
    kind: ValueKind::KvCacheType,
  },
  KnobSpec {
    field: KnobField::Parallel,
    canonical: "--parallel",
    aliases: &["-np"],
    kind: ValueKind::U32,
  },
  KnobSpec {
    field: KnobField::FlashAttn,
    canonical: "--flash-attn",
    aliases: &["-fa"],
    kind: ValueKind::Bool,
  },
  KnobSpec {
    field: KnobField::Mlock,
    canonical: "--mlock",
    aliases: &[],
    kind: ValueKind::Bool,
  },
  KnobSpec {
    field: KnobField::NoMmap,
    canonical: "--no-mmap",
    aliases: &[],
    kind: ValueKind::Bool,
  },
  KnobSpec {
    field: KnobField::BatchSize,
    canonical: "--batch-size",
    aliases: &["-b"],
    kind: ValueKind::U32,
  },
  KnobSpec {
    field: KnobField::UbatchSize,
    canonical: "--ubatch-size",
    aliases: &["-ub"],
    kind: ValueKind::U32,
  },
  KnobSpec {
    field: KnobField::RopeFreqScale,
    canonical: "--rope-freq-scale",
    aliases: &[],
    kind: ValueKind::F32,
  },
  KnobSpec {
    field: KnobField::Keep,
    canonical: "--keep",
    aliases: &[],
    kind: ValueKind::U32,
  },
];

/// All knob specs in canonical emission order. Used by `argvify` and
/// the typed editor to render rows top-to-bottom.
pub fn knob_specs() -> &'static [KnobSpec] {
  SPECS
}

/// Lookup a spec by field.
pub fn spec_for(field: KnobField) -> &'static KnobSpec {
  SPECS
    .iter()
    .find(|s| s.field == field)
    .expect("knob_specs is exhaustive over KnobField")
}

/// Result of `recognise` — what the token matched, and whether the
/// value was already inlined via `=`.
#[derive(Debug, PartialEq, Eq)]
pub struct Recognised<'a> {
  pub field: KnobField,
  pub kind: ValueKind,
  /// `Some(v)` when the caller passed `--flag=value`. `None` means
  /// "consume the next argv token as the value" (or no value for
  /// `ValueKind::Bool`).
  pub inline_value: Option<&'a str>,
}

/// Match a single argv token against the alias table. Returns `None`
/// for unrecognised flags so the caller can route them to `extras`.
///
/// Match is case-insensitive on the flag head (canonical / aliases
/// are lowercase already; user-typed flags get lowercased before
/// comparison).
pub fn recognise(token: &str) -> Option<Recognised<'_>> {
  let (head, inline) = match token.split_once('=') {
    Some((h, v)) => (h, Some(v)),
    None => (token, None),
  };
  let lower = head.to_ascii_lowercase();
  for spec in SPECS {
    if spec.canonical == lower || spec.aliases.iter().any(|a| *a == lower) {
      return Some(Recognised {
        field: spec.field,
        kind: spec.kind,
        inline_value: inline,
      });
    }
  }
  None
}

/// True when `token`'s head (before any `=`) is one of the canonical
/// flag names or short aliases for `field`. Used by `argvify` to
/// avoid duplicating a flag the caller already supplied in `extras`.
#[allow(dead_code)]
pub fn token_matches(token: &str, field: KnobField) -> bool {
  let head = token
    .split('=')
    .next()
    .unwrap_or(token)
    .to_ascii_lowercase();
  let spec = spec_for(field);
  spec.canonical == head || spec.aliases.iter().any(|a| *a == head)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn recognise_canonical_long_flag() {
    let r = recognise("--threads").unwrap();
    assert_eq!(r.field, KnobField::Threads);
    assert_eq!(r.kind, ValueKind::U32);
    assert_eq!(r.inline_value, None);
  }

  #[test]
  fn recognise_short_alias() {
    let r = recognise("-ngl").unwrap();
    assert_eq!(r.field, KnobField::NGpuLayers);
  }

  #[test]
  fn recognise_equals_form_splits_value() {
    let r = recognise("--threads=8").unwrap();
    assert_eq!(r.field, KnobField::Threads);
    assert_eq!(r.inline_value, Some("8"));
  }

  #[test]
  fn recognise_unknown_flag_returns_none() {
    assert!(recognise("--rope-freq-base").is_none());
    assert!(recognise("--ssl-key-file").is_none());
  }

  #[test]
  fn recognise_is_case_insensitive_on_flag_head() {
    let r = recognise("--THREADS=8").unwrap();
    assert_eq!(r.field, KnobField::Threads);
    assert_eq!(r.inline_value, Some("8"));
  }

  #[test]
  fn token_matches_accepts_canonical_and_aliases() {
    assert!(token_matches("--threads", KnobField::Threads));
    assert!(token_matches("-t", KnobField::Threads));
    assert!(token_matches("--threads=8", KnobField::Threads));
    assert!(!token_matches("--threads", KnobField::NGpuLayers));
  }

  #[test]
  fn knob_specs_pinned_order_covers_every_field() {
    let canon: Vec<&str> = knob_specs().iter().map(|s| s.canonical).collect();
    assert_eq!(
      canon,
      vec![
        "--n-gpu-layers",
        "--threads",
        "--cache-type-k",
        "--cache-type-v",
        "--parallel",
        "--flash-attn",
        "--mlock",
        "--no-mmap",
        "--batch-size",
        "--ubatch-size",
        "--rope-freq-scale",
        "--keep",
      ]
    );
  }
}
