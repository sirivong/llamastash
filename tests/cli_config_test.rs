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
