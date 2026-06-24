//! ONE-TIME MIGRATION (remove after v0.2.0) — see TODO.md.
//!
//! Before this feature, named presets lived in `state.json`. They now live
//! in `config.yaml` (the single writable source). On the first daemon start
//! after the upgrade this imports any legacy `state.json` presets into
//! `config.yaml` under model-basename keys (config wins on collision), then
//! clears the `state.json` `presets` field so the import never re-runs.
//!
//! When the migration window closes, delete this whole module, its call in
//! [`crate::daemon::run_foreground`], and the now-dead `state.json`
//! `presets` field (`DaemonState.presets` + `PresetsEntry`).

use std::collections::BTreeMap;
use std::path::Path;

use crate::config::{presets_writer, ConfigPresetBlock};
use crate::daemon::state_store::DaemonState;
use crate::launch::presets::preset_body_from_launch_params;

/// Import `state.presets` into `config_presets` (and, when `config_path`
/// is set, into `config.yaml`), keyed by each model's basename. An entry
/// name already present under its key is kept untouched (config wins, and
/// a prior run's persisted entries aren't rewritten). The `state.json`
/// `presets` field is cleared only when every entry persisted durably, so
/// a transient write error retries the *remaining* entries on the next
/// boot rather than silently dropping data. Returns the number migrated.
pub fn migrate_state_presets_to_config(
  state: &mut DaemonState,
  config_presets: &mut BTreeMap<String, ConfigPresetBlock>,
  config_path: Option<&Path>,
) -> usize {
  if state.presets.is_empty() {
    return 0;
  }

  let mut migrated = 0usize;
  // Durable persistence needs a config file; without one (tests) we keep
  // state.presets so nothing is lost on the next, real boot.
  let mut all_persisted = config_path.is_some();

  for entry in &state.presets {
    let key = entry.id.display_name();
    for np in entry.presets.iter() {
      // Skip names already present under this key. Config-authored entries
      // win, and an entry a prior run already persisted isn't rewritten.
      // The check is per *entry*, not per model key, on purpose: if a
      // partial run persisted some of a model's entries (the key now
      // exists in config), the remaining entries must still migrate on the
      // retry rather than the whole model being skipped — otherwise the
      // un-persisted entries would be dropped when `state.presets` clears.
      // It also merges two `state.json` entries that share a basename (the
      // same filename in two dirs) into one key instead of dropping the
      // second model's presets.
      if config_presets
        .get(&key)
        .is_some_and(|b| b.entries.contains_key(&np.name))
      {
        continue;
      }
      let body = preset_body_from_launch_params(&np.params);
      if let Some(path) = config_path {
        if let Err(e) = presets_writer::upsert_preset(path, &key, &np.name, &body) {
          log::warn!(
            "preset migration: failed to write `{key}` / `{}`: {e}",
            np.name
          );
          all_persisted = false;
          continue;
        }
      }
      config_presets
        .entry(key.clone())
        .or_default()
        .entries
        .insert(np.name.clone(), body);
      migrated += 1;
    }
  }

  if all_persisted {
    state.presets.clear();
  }
  if migrated > 0 {
    log::info!("migrated {migrated} preset(s) from state.json into config.yaml");
  }
  migrated
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::backend::identity::ModelIdentity;
  use crate::config::KnobValue;
  use crate::daemon::state_store::PresetsEntry;
  use crate::gguf::identity::ModelId;
  use crate::launch::mode::LaunchMode;
  use crate::launch::params::LaunchParams;
  use crate::launch::presets::{NamedPreset, Presets};
  use std::path::PathBuf;

  fn gguf_id(path: &str) -> ModelIdentity {
    ModelIdentity::Gguf(ModelId {
      path: PathBuf::from(path),
      header_blake3: [7; 32],
    })
  }

  fn preset(name: &str, ctx: u32) -> NamedPreset {
    let mut params = LaunchParams::new(PathBuf::from("/unused"), LaunchMode::Chat);
    params.ctx = Some(ctx);
    NamedPreset {
      name: name.into(),
      params,
    }
  }

  fn entry(id: ModelIdentity, presets: &[NamedPreset]) -> PresetsEntry {
    let mut p = Presets::new();
    for np in presets {
      p.upsert(np.clone());
    }
    PresetsEntry { id, presets: p }
  }

  fn temp_config(label: &str) -> PathBuf {
    crate::util::test_temp::unique_temp_dir(&format!("preset-migration-{label}"))
      .join("config.yaml")
  }

  #[test]
  fn migrates_two_models_and_clears_state() {
    let path = temp_config("happy");
    let mut state = DaemonState::default();
    state
      .presets
      .push(entry(gguf_id("/m/a.gguf"), &[preset("p1", 8192)]));
    state
      .presets
      .push(entry(gguf_id("/m/b.gguf"), &[preset("p2", 4096)]));
    let mut config_presets = BTreeMap::new();

    let n = migrate_state_presets_to_config(&mut state, &mut config_presets, Some(&path));
    assert_eq!(n, 2);
    assert!(state.presets.is_empty(), "state.json presets cleared");
    // In-memory map populated by basename.
    assert_eq!(
      config_presets["a.gguf"].entries["p1"].knobs.ctx,
      Some(KnobValue::Set(8192))
    );
    // And durably on disk.
    let cfg = crate::config::load_config_from_path(&path).config;
    assert_eq!(
      cfg.presets["b.gguf"].entries["p2"].knobs.ctx,
      Some(KnobValue::Set(4096))
    );
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
  }

  #[test]
  fn two_state_entries_sharing_a_basename_merge_their_names() {
    // The same filename in two dirs, each with its own preset: both names
    // must survive (merged into one basename key), not silently dropped.
    let path = temp_config("dup-basename");
    let mut state = DaemonState::default();
    state
      .presets
      .push(entry(gguf_id("/a/model.gguf"), &[preset("p_a", 1)]));
    state
      .presets
      .push(entry(gguf_id("/b/model.gguf"), &[preset("p_b", 2)]));
    let mut config_presets = BTreeMap::new();
    let n = migrate_state_presets_to_config(&mut state, &mut config_presets, Some(&path));
    assert_eq!(n, 2, "both presets migrated");
    let entries = &config_presets["model.gguf"].entries;
    assert!(
      entries.contains_key("p_a") && entries.contains_key("p_b"),
      "both names kept"
    );
    assert!(state.presets.is_empty());
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
  }

  #[test]
  fn config_wins_on_collision() {
    let path = temp_config("collision");
    // config.yaml already has an a.gguf preset — must not be clobbered.
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
      &path,
      "presets:\n  a.gguf:\n    entries:\n      p1: { ctx: 100 }\n",
    )
    .unwrap();
    let mut config_presets = crate::config::load_config_from_path(&path)
      .config
      .presets
      .clone();
    let mut state = DaemonState::default();
    state
      .presets
      .push(entry(gguf_id("/m/a.gguf"), &[preset("p1", 999)]));

    migrate_state_presets_to_config(&mut state, &mut config_presets, Some(&path));
    let cfg = crate::config::load_config_from_path(&path).config;
    assert_eq!(
      cfg.presets["a.gguf"].entries["p1"].knobs.ctx,
      Some(KnobValue::Set(100)),
      "existing config value wins"
    );
    assert!(state.presets.is_empty(), "state still cleared");
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
  }

  #[test]
  fn retry_after_partial_write_completes_remaining_entries() {
    // Simulate a prior run that persisted p1 but failed on p2: config.yaml
    // has p1, state.json still carries both (it wasn't cleared). The retry
    // must migrate the missing p2 — not skip the whole model and then drop
    // p2 when state clears.
    let path = temp_config("partial-retry");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
      &path,
      "presets:\n  a.gguf:\n    entries:\n      p1: { ctx: 8192 }\n",
    )
    .unwrap();
    let mut config_presets = crate::config::load_config_from_path(&path)
      .config
      .presets
      .clone();
    let mut state = DaemonState::default();
    state.presets.push(entry(
      gguf_id("/m/a.gguf"),
      &[preset("p1", 8192), preset("p2", 4096)],
    ));

    let n = migrate_state_presets_to_config(&mut state, &mut config_presets, Some(&path));
    assert_eq!(n, 1, "only the missing p2 migrates");
    let cfg = crate::config::load_config_from_path(&path).config;
    assert_eq!(
      cfg.presets["a.gguf"].entries["p1"].knobs.ctx,
      Some(KnobValue::Set(8192)),
      "already-persisted p1 untouched"
    );
    assert_eq!(
      cfg.presets["a.gguf"].entries["p2"].knobs.ctx,
      Some(KnobValue::Set(4096)),
      "p2 recovered, not dropped"
    );
    assert!(
      state.presets.is_empty(),
      "state cleared after full migration"
    );
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
  }

  #[test]
  fn second_run_is_a_noop() {
    let path = temp_config("idempotent");
    let mut state = DaemonState::default();
    state
      .presets
      .push(entry(gguf_id("/m/a.gguf"), &[preset("p1", 8192)]));
    let mut config_presets = BTreeMap::new();
    migrate_state_presets_to_config(&mut state, &mut config_presets, Some(&path));
    // Second boot: state already cleared → nothing to do, config untouched.
    let before = std::fs::read_to_string(&path).unwrap();
    let n = migrate_state_presets_to_config(&mut state, &mut config_presets, Some(&path));
    assert_eq!(n, 0);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
  }

  #[test]
  fn missing_model_file_still_keyed_by_basename() {
    let path = temp_config("gone");
    let mut state = DaemonState::default();
    // The file no longer exists on disk; the stored path's basename is
    // still the key.
    state
      .presets
      .push(entry(gguf_id("/deleted/ghost.gguf"), &[preset("p", 2048)]));
    let mut config_presets = BTreeMap::new();
    migrate_state_presets_to_config(&mut state, &mut config_presets, Some(&path));
    assert!(config_presets.contains_key("ghost.gguf"));
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
  }

  #[test]
  fn empty_state_presets_writes_nothing() {
    let path = temp_config("empty");
    let mut state = DaemonState::default();
    let mut config_presets = BTreeMap::new();
    let n = migrate_state_presets_to_config(&mut state, &mut config_presets, Some(&path));
    assert_eq!(n, 0);
    assert!(
      !path.exists(),
      "no config file created for an empty migration"
    );
  }
}
