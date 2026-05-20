//! Named launch presets per model (`R21`).
//!
//! Stored under `state_dir/state.json` as
//! `presets: HashMap<ModelId, Vec<NamedPreset>>`. A preset's `params`
//! is a full [`LaunchParams`] snapshot so applying a preset is just
//! "clone these params, then layer per-invocation overrides on top".

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::gguf::identity::ModelId;
use crate::launch::params::LaunchParams;

/// One saved preset. `name` is unique within a single model's preset
/// list and is the handle users type at the CLI / TUI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedPreset {
  pub name: String,
  pub params: LaunchParams,
}

/// Per-model preset list. Wrapper around `Vec<NamedPreset>` so the
/// state_store API stays clear about insertion / removal semantics.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Presets {
  entries: Vec<NamedPreset>,
}

impl Presets {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn len(&self) -> usize {
    self.entries.len()
  }

  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  pub fn iter(&self) -> std::slice::Iter<'_, NamedPreset> {
    self.entries.iter()
  }

  /// Insert or replace a preset by name. Returns the previous entry
  /// (if any) so callers can surface "renamed" / "overwrote" messages.
  pub fn upsert(&mut self, preset: NamedPreset) -> Option<NamedPreset> {
    if let Some(existing) = self.entries.iter_mut().find(|p| p.name == preset.name) {
      let old = std::mem::replace(existing, preset);
      Some(old)
    } else {
      self.entries.push(preset);
      None
    }
  }

  /// Remove a preset by name. Returns the entry if it existed.
  pub fn remove(&mut self, name: &str) -> Option<NamedPreset> {
    let idx = self.entries.iter().position(|p| p.name == name)?;
    Some(self.entries.remove(idx))
  }

  pub fn get(&self, name: &str) -> Option<&NamedPreset> {
    self.entries.iter().find(|p| p.name == name)
  }
}

/// Top-level map from canonical model id → that model's preset list.
/// Serialised in `state.json` under `presets`.
pub type PresetStore = BTreeMap<ModelId, Presets>;

#[cfg(test)]
mod tests {
  use super::*;

  use std::path::PathBuf;

  use crate::launch::mode::LaunchMode;

  fn p(name: &str) -> NamedPreset {
    NamedPreset {
      name: name.to_string(),
      params: LaunchParams::new(PathBuf::from("/m/a.gguf"), LaunchMode::Chat),
    }
  }

  #[test]
  fn upsert_inserts_then_replaces() {
    let mut store = Presets::new();
    assert!(store.upsert(p("coding")).is_none());
    let mut alt = p("coding");
    alt.params.ctx = Some(32768);
    let prev = store.upsert(alt.clone()).expect("returns previous entry");
    assert!(prev.params.ctx.is_none());
    assert_eq!(store.len(), 1, "no duplicate");
    assert_eq!(store.get("coding"), Some(&alt));
  }

  #[test]
  fn remove_returns_entry_and_none_for_unknown() {
    let mut store = Presets::new();
    store.upsert(p("coding"));
    let removed = store.remove("coding");
    assert!(removed.is_some());
    assert!(store.is_empty());
    assert!(store.remove("nope").is_none());
  }

  #[test]
  fn json_round_trip_preserves_order() {
    let mut store = Presets::new();
    store.upsert(p("a"));
    store.upsert(p("b"));
    let json = serde_json::to_string(&store).unwrap();
    let back: Presets = serde_json::from_str(&json).unwrap();
    assert_eq!(back, store);
    let names: Vec<_> = back.iter().map(|p| p.name.clone()).collect();
    assert_eq!(names, vec!["a", "b"]);
  }
}
