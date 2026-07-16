use std::process::Command;

#[test]
fn config_opens_a_missing_explicit_path_with_editor() {
  let dir = tempfile::tempdir().unwrap();
  let config = dir.path().join("config.yaml");
  let output = Command::new(env!("CARGO_BIN_EXE_llamastash"))
    .args(["--config", config.to_str().unwrap(), "config"])
    .env("EDITOR", "true")
    .output()
    .unwrap();

  assert!(
    output.status.success(),
    "stderr: {}",
    String::from_utf8_lossy(&output.stderr)
  );
}

#[test]
fn config_explains_when_editor_is_unset() {
  let dir = tempfile::tempdir().unwrap();
  let config = dir.path().join("config.yaml");
  let output = Command::new(env!("CARGO_BIN_EXE_llamastash"))
    .args(["--config", config.to_str().unwrap(), "config"])
    .env_remove("EDITOR")
    .output()
    .unwrap();

  assert_eq!(output.status.code(), Some(64));
  assert!(
    String::from_utf8_lossy(&output.stderr).contains("the EDITOR environment variable is not set")
  );
}

#[test]
fn config_opens_a_malformed_file_for_repair() {
  let dir = tempfile::tempdir().unwrap();
  let config = dir.path().join("config.yaml");
  std::fs::write(&config, "proxy: [not valid").unwrap();
  let output = Command::new(env!("CARGO_BIN_EXE_llamastash"))
    .args(["--config", config.to_str().unwrap(), "config"])
    .env("EDITOR", "true")
    .output()
    .unwrap();

  assert!(
    output.status.success(),
    "stderr: {}",
    String::from_utf8_lossy(&output.stderr)
  );
}

#[test]
fn config_bindings_prints_every_effective_binding_as_yaml() {
  let dir = tempfile::tempdir().unwrap();
  let config = dir.path().join("config.yaml");
  std::fs::write(&config, "keybindings:\n  toggle_help: f1\n  quit: ctrl+q\n").unwrap();
  let output = Command::new(env!("CARGO_BIN_EXE_llamastash"))
    .args(["--config", config.to_str().unwrap(), "config", "bindings"])
    .output()
    .unwrap();

  assert!(
    output.status.success(),
    "stderr: {}",
    String::from_utf8_lossy(&output.stderr)
  );
  let stdout = String::from_utf8(output.stdout).unwrap();
  assert!(stdout.starts_with("keybindings:\n"));
  assert!(stdout.contains("  quit: ctrl+q\n"));
  assert!(stdout.contains("  toggle_help: f1\n"));
  assert!(stdout.contains("  move_up: up\n"));
  assert!(stdout.contains("  stop_model: ctrl+s\n"));
}

#[test]
fn config_bindings_prints_defaults_when_no_overrides_exist() {
  let dir = tempfile::tempdir().unwrap();
  let config = dir.path().join("config.yaml");
  let output = Command::new(env!("CARGO_BIN_EXE_llamastash"))
    .args(["--config", config.to_str().unwrap(), "config", "bindings"])
    .output()
    .unwrap();

  assert!(
    output.status.success(),
    "stderr: {}",
    String::from_utf8_lossy(&output.stderr)
  );
  let stdout = String::from_utf8(output.stdout).unwrap();
  assert!(stdout.starts_with("keybindings:\n"));
  assert!(stdout.contains("  quit: q\n"));
  assert!(stdout.contains("  toggle_help: '?'\n"));
  assert!(stdout.contains("  move_up: up\n"));
}

#[test]
fn config_bindings_rejects_a_malformed_source_config() {
  let dir = tempfile::tempdir().unwrap();
  let config = dir.path().join("config.yaml");
  std::fs::write(&config, "keybindings: [not valid").unwrap();
  let output = Command::new(env!("CARGO_BIN_EXE_llamastash"))
    .args(["--config", config.to_str().unwrap(), "config", "bindings"])
    .output()
    .unwrap();

  assert_eq!(output.status.code(), Some(64));
  assert!(String::from_utf8_lossy(&output.stderr).contains("config error:"));
  assert!(output.stdout.is_empty());
}
