//! Integration coverage for the one-time `state.json` → `config.yaml`
//! preset migration that runs at daemon start.
//!
//! ONE-TIME MIGRATION test — remove together with `preset_migration`.

#![cfg(feature = "test-fixtures")]

use std::path::{Path, PathBuf};
use std::time::Duration;

use llamastash::backend::identity::ModelIdentity;
use llamastash::config::load_config_from_path;
use llamastash::daemon::state_store::{self, DaemonState, PresetsEntry};
use llamastash::daemon::{run_foreground, DaemonOptions};
use llamastash::gguf::identity::ModelId;
use llamastash::ipc::Client;
use llamastash::launch::mode::LaunchMode;
use llamastash::launch::params::LaunchParams;
use llamastash::launch::presets::{NamedPreset, Presets};
use tokio::time::timeout;

fn unique_temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-migrate", label)
}

async fn wait_for_socket(path: &Path) {
  let deadline = std::time::Instant::now() + Duration::from_secs(5);
  loop {
    if std::time::Instant::now() > deadline {
      panic!("daemon did not become connectable: {}", path.display());
    }
    if Client::connect(path).await.is_ok() {
      return;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
}

fn preset(name: &str, ctx: u32) -> NamedPreset {
  let mut params = LaunchParams::new(PathBuf::from("/m/coder.gguf"), LaunchMode::Chat);
  params.ctx = Some(ctx);
  NamedPreset {
    name: name.into(),
    params,
  }
}

fn seed_state_with_presets(state_dir: &Path) {
  let mut presets = Presets::new();
  presets.upsert(preset("short-ctx", 8192));
  presets.upsert(preset("long-ctx", 65536));
  let mut ds = DaemonState::default();
  ds.presets.push(PresetsEntry {
    id: ModelIdentity::Gguf(ModelId {
      path: PathBuf::from("/m/coder.gguf"),
      header_blake3: [9; 32],
    }),
    presets,
  });
  state_store::save(state_dir, &ds).expect("seed state.json");
}

/// Boot the daemon once against `state` + `config_path`, wait until it is
/// connectable (migration has run by then), then shut it down cleanly.
async fn boot_then_shutdown(state: &Path, config_path: &Path) {
  let mut opts = DaemonOptions::rooted_at(state.to_path_buf());
  opts.config_path = Some(config_path.to_path_buf());
  // Keep the proxy off so parallel test daemons don't contend on a port.
  opts.proxy.enabled = false;
  let socket = opts.state_dir.clone();
  let handle = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;
  let mut client = Client::connect(&socket).await.expect("connect");
  client.call("shutdown", None).await.expect("shutdown");
  timeout(Duration::from_secs(3), handle)
    .await
    .expect("daemon exits")
    .expect("join")
    .expect("daemon result");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boot_migrates_state_presets_into_config_then_clears_state() {
  let state = unique_temp("state");
  let cfg_dir = unique_temp("cfg");
  let config_path = cfg_dir.join("config.yaml");
  seed_state_with_presets(&state);

  boot_then_shutdown(&state, &config_path).await;

  // Presets landed in config.yaml under the model basename.
  let cfg = load_config_from_path(&config_path).config;
  let block = cfg
    .presets
    .get("coder.gguf")
    .expect("migrated model key present in config");
  assert_eq!(block.entries.len(), 2);
  assert!(block.entries.contains_key("short-ctx"));
  assert!(block.entries.contains_key("long-ctx"));

  // state.json presets cleared so the migration never re-runs.
  let reloaded = state_store::load(&state).expect("reload state");
  assert!(reloaded.presets.is_empty(), "state.json presets cleared");

  // Second boot is a no-op: config unchanged, state still empty.
  let before = std::fs::read_to_string(&config_path).unwrap();
  boot_then_shutdown(&state, &config_path).await;
  assert_eq!(std::fs::read_to_string(&config_path).unwrap(), before);
  assert!(state_store::load(&state).unwrap().presets.is_empty());

  std::fs::remove_dir_all(&state).ok();
  std::fs::remove_dir_all(&cfg_dir).ok();
}
