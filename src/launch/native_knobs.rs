//! Per-backend **native knobs** — a generic, string-id-keyed tuning channel
//! parallel to the llama.cpp [`KnobField`](crate::launch::flag_aliases::KnobField)
//! IR.
//!
//! A backend's real tunables may live *outside* the shared llama.cpp-keyed
//! IR (R4). This channel lets a backend declare its own knobs
//! ([`Backend::native_knobs`](crate::backend::Backend::native_knobs), default
//! empty), which the launch picker renders as backend-filtered cycle/edit
//! rows, persists in [`LaunchParams.backend_knobs`](crate::launch::params::LaunchParams)
//! / presets, and translates to flags in the backend's `prepare_launch` via
//! [`translate`].
//!
//! It is `capabilities()`-orthogonal: a backend can honor no
//! [`KnobField`](crate::launch::flag_aliases::KnobField) and
//! still declare a non-empty native-knob set. ds4 is the first real consumer
//! (its `--power`/`--kv-disk-*`/`--ssd-streaming`/… tunables have no llama.cpp
//! IR slot); llama.cpp and Lemonade return an empty set so the picker +
//! persistence stay byte-identical for them.

use std::collections::BTreeMap;
use std::ffi::OsString;

use crate::config::KnobValue;
use crate::launch::params::is_forbidden_head_ext;

/// How a native knob is surfaced in the picker and how its value behaves.
///
/// Stored values are always `String` (the backend interprets them); the kind
/// only drives the picker affordance + the value-vs-bare-flag emission shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeKnobKind {
  /// `←/→` cycles a closed preset ring (`inherited → presets… → wrap`).
  Cycle { presets: &'static [&'static str] },
  /// `e`-edit a free-text value.
  FreeText,
  /// `←/→` toggles `inherited → on → off`. Emits a bare flag when on.
  Bool,
}

/// One backend-declared native knob. Mirrors
/// [`KnobSpec`](crate::launch::flag_aliases::KnobSpec)'s shape but lives
/// outside the llama.cpp IR. `id` is the stable persistence/wire key (the
/// `backend_knobs` map key); `label` / `description` drive the picker row.
#[derive(Debug, Clone, Copy)]
pub struct NativeKnobDescriptor {
  pub id: &'static str,
  pub label: &'static str,
  pub description: &'static str,
  pub kind: NativeKnobKind,
}

impl NativeKnobDescriptor {
  /// Whether `e`-edit opens an inline buffer (free-text only). Cycle / bool
  /// rows are `←/→`-only, matching the typed-knob picker's rule.
  pub fn is_editable(&self) -> bool {
    matches!(self.kind, NativeKnobKind::FreeText)
  }

  /// Whether this is a boolean toggle (emits a bare flag, no value).
  pub fn is_bool(&self) -> bool {
    matches!(self.kind, NativeKnobKind::Bool)
  }

  /// The cycle ring for a `Cycle` knob; empty for the other kinds.
  pub fn cycle_presets(&self) -> &'static [&'static str] {
    match self.kind {
      NativeKnobKind::Cycle { presets } => presets,
      _ => &[],
    }
  }
}

/// Translate a backend's set native-knob values into `llama-server`-style
/// argv tokens, applying the **same** loopback/credential strip `compose`
/// enforces on extras.
///
/// - `descriptors` declares each knob's id + kind (a `Bool` emits a bare
///   flag when on; the others emit `flag value`).
/// - `flag_map` is the backend's `id → flag-head` mapping (built in its
///   `prepare_launch`); a knob with no mapping is skipped.
/// - `values` is the resolved per-knob state. Only `Set(v)` emits; `Auto` /
///   unset / `None` emit nothing (the deferred resolver layering would seed
///   these — MVP is user-set-or-nothing).
///
/// A knob whose flag head **or** value head hits the denylist (the shared
/// `is_forbidden_head` guard `compose` applies to extras, plus the backend's
/// own `extra_forbidden` heads — e.g. ds4's `--cors` / `--dist-`) is dropped
/// and logged, so no backend's free-text value can rebind the listener off
/// loopback or weaken its network posture.
pub fn translate(
  descriptors: &[NativeKnobDescriptor],
  flag_map: &[(&str, &str)],
  values: &BTreeMap<String, KnobValue<String>>,
  extra_forbidden: &[&str],
) -> Vec<OsString> {
  let mut out: Vec<OsString> = Vec::new();
  for d in descriptors {
    let Some(flag) = flag_map.iter().find(|(id, _)| *id == d.id).map(|(_, f)| *f) else {
      continue;
    };
    // Only an explicitly-Set value emits (Auto / unset / None → nothing).
    let Some(KnobValue::Set(value)) = values.get(d.id) else {
      continue;
    };
    if d.is_bool() {
      // A bool emits a bare flag when on; "false" / anything else emits
      // nothing (no `--no-flag` form, matching the typed-knob bools).
      if value == "true" {
        push_checked(&mut out, flag, None, extra_forbidden);
      }
    } else if !value.is_empty() {
      push_checked(&mut out, flag, Some(value), extra_forbidden);
    }
  }
  out
}

/// Push `flag` (+ optional `value`) unless either would rebind off loopback
/// or hit the backend's extra denylist.
fn push_checked(
  out: &mut Vec<OsString>,
  flag: &str,
  value: Option<&str>,
  extra_forbidden: &[&str],
) {
  if is_forbidden_head_ext(flag, extra_forbidden) {
    log::warn!("native_knobs: stripping forbidden flag {flag:?}");
    return;
  }
  if let Some(v) = value {
    // A free-text value could smuggle a forbidden flag as a space- OR
    // `=`-separated token (`--host 0.0.0.0`, `--host=0.0.0.0`). A native value
    // is one untokenized string (unlike `compose`'s pre-split argv extras), so
    // split on whitespace first, then on `=`. The denylist itself is the shared
    // `is_forbidden_head_ext` leaf `compose` uses — that single source is what
    // keeps the two strips in lockstep.
    let smuggles = v
      .split_whitespace()
      .any(|tok| is_forbidden_head_ext(tok.split('=').next().unwrap_or(tok), extra_forbidden));
    if smuggles {
      log::warn!("native_knobs: stripping forbidden value head in {v:?}");
      return;
    }
  }
  out.push(OsString::from(flag));
  if let Some(v) = value {
    out.push(OsString::from(v));
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A representative stub descriptor slice: one Cycle + one FreeText + one
  /// Bool. The mechanism is proven against this in addition to ds4's real
  /// descriptor table.
  pub(crate) const STUB: &[NativeKnobDescriptor] = &[
    NativeKnobDescriptor {
      id: "kv_bits",
      label: "KV cache bits",
      description: "quantization width for the KV cache",
      kind: NativeKnobKind::Cycle {
        presets: &["4", "8", "16"],
      },
    },
    NativeKnobDescriptor {
      id: "adapter_path",
      label: "Adapter path",
      description: "path to a LoRA adapter",
      kind: NativeKnobKind::FreeText,
    },
    NativeKnobDescriptor {
      id: "trust_remote",
      label: "Trust remote code",
      description: "allow custom model code",
      kind: NativeKnobKind::Bool,
    },
  ];

  fn set(v: &str) -> KnobValue<String> {
    KnobValue::Set(v.to_string())
  }

  #[test]
  fn translate_emits_flag_value_pairs_in_descriptor_order() {
    let flags = &[("kv_bits", "--kv-bits"), ("adapter_path", "--adapter")];
    let mut values = BTreeMap::new();
    values.insert("kv_bits".to_string(), set("8"));
    values.insert("adapter_path".to_string(), set("./x"));
    let argv = translate(STUB, flags, &values, &[]);
    let got: Vec<String> = argv
      .iter()
      .map(|s| s.to_string_lossy().into_owned())
      .collect();
    assert_eq!(got, vec!["--kv-bits", "8", "--adapter", "./x"]);
  }

  #[test]
  fn translate_bool_emits_bare_flag_when_on_and_nothing_when_off() {
    let flags = &[("trust_remote", "--trust-remote-code")];
    let mut on = BTreeMap::new();
    on.insert("trust_remote".to_string(), set("true"));
    let argv = translate(STUB, flags, &on, &[]);
    let got: Vec<String> = argv
      .iter()
      .map(|s| s.to_string_lossy().into_owned())
      .collect();
    assert_eq!(got, vec!["--trust-remote-code"]);

    let mut off = BTreeMap::new();
    off.insert("trust_remote".to_string(), set("false"));
    assert!(
      translate(STUB, flags, &off, &[]).is_empty(),
      "off → no flag"
    );
  }

  #[test]
  fn translate_unset_or_auto_knob_emits_nothing() {
    let flags = &[("kv_bits", "--kv-bits")];
    // Unset.
    assert!(translate(STUB, flags, &BTreeMap::new(), &[]).is_empty());
    // Auto.
    let mut auto = BTreeMap::new();
    auto.insert("kv_bits".to_string(), KnobValue::Auto);
    assert!(
      translate(STUB, flags, &auto, &[]).is_empty(),
      "Auto → no flag"
    );
  }

  #[test]
  fn translate_strips_value_smuggling_a_forbidden_flag() {
    // A free-text value that tries to rebind off loopback is dropped — in
    // the space form, the `=` form, mixed case, and as a non-leading token,
    // so the native strip matches `compose`'s extras strip byte-for-byte.
    let flags = &[("adapter_path", "--adapter")];
    for smuggle in [
      "--host 0.0.0.0",
      "--host=0.0.0.0",
      "--HOST=0.0.0.0",
      "--listen=0.0.0.0:8080",
      "--bind=0.0.0.0",
      "--api-key=leak",
      "--ssl-key-file=/tmp/k",
      "ok --api-key=leak", // forbidden head as a non-leading token
    ] {
      let mut values = BTreeMap::new();
      values.insert("adapter_path".to_string(), set(smuggle));
      assert!(
        translate(STUB, flags, &values, &[]).is_empty(),
        "value {smuggle:?} must be stripped"
      );
    }
    // A benign value with no forbidden head still emits.
    let mut ok = BTreeMap::new();
    ok.insert("adapter_path".to_string(), set("./lora"));
    assert_eq!(
      translate(STUB, flags, &ok, &[]),
      vec![OsString::from("--adapter"), OsString::from("./lora")]
    );
  }

  #[test]
  fn translate_strips_flag_head_on_denylist() {
    // A backend that maps a knob directly onto a forbidden flag is refused.
    let flags = &[("adapter_path", "--api-key")];
    let mut values = BTreeMap::new();
    values.insert("adapter_path".to_string(), set("secret"));
    assert!(translate(STUB, flags, &values, &[]).is_empty());
  }

  #[test]
  fn translate_honors_backend_extra_forbidden_heads() {
    // ds4 extends the base denylist with `--cors` / `--dist-`; a knob mapped
    // onto (or a value smuggling) one of those is stripped even though the
    // base set allows it.
    let extra = &["--cors", "--dist-"];
    // Flag head hits the extra set.
    let flags = &[("trust_remote", "--cors")];
    let mut on = BTreeMap::new();
    on.insert("trust_remote".to_string(), set("true"));
    assert!(
      translate(STUB, flags, &on, extra).is_empty(),
      "--cors flag head stripped"
    );
    // Value smuggles a `--dist-` head.
    let flags2 = &[("adapter_path", "--adapter")];
    let mut v = BTreeMap::new();
    v.insert("adapter_path".to_string(), set("--dist-worker 1.2.3.4"));
    assert!(
      translate(STUB, flags2, &v, extra).is_empty(),
      "--dist- value head stripped"
    );
    // A benign value still emits under the extended set.
    let mut ok = BTreeMap::new();
    ok.insert("adapter_path".to_string(), set("./ok"));
    assert_eq!(
      translate(STUB, flags2, &ok, extra),
      vec![OsString::from("--adapter"), OsString::from("./ok")]
    );
  }

  #[test]
  fn translate_skips_knob_with_no_flag_mapping() {
    let mut values = BTreeMap::new();
    values.insert("kv_bits".to_string(), set("8"));
    // Empty flag map → nothing to emit, no panic.
    assert!(translate(STUB, &[], &values, &[]).is_empty());
  }

  #[test]
  fn descriptor_affordances_track_kind() {
    assert!(STUB[1].is_editable(), "FreeText is editable");
    assert!(!STUB[0].is_editable(), "Cycle is cycle-only");
    assert!(!STUB[2].is_editable(), "Bool is cycle-only");
    assert!(STUB[2].is_bool());
    assert_eq!(STUB[0].cycle_presets(), &["4", "8", "16"]);
    assert!(STUB[1].cycle_presets().is_empty());
  }
}
