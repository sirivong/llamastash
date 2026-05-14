use std::{
  collections::BTreeMap,
  env,
  ffi::OsString,
  fs,
  io::ErrorKind,
  path::{Path, PathBuf},
};

use log::warn;
use serde::{Deserialize, Serialize};

use crate::theme::ThemeName;
use crate::util::paths::user_config_file;

/// User-authored YAML config, with sensible defaults via `#[serde(default)]`.
///
/// Every field is optional in the file; missing fields use the built-in
/// defaults. Unknown fields are accepted silently so old files keep working
/// when new fields are added (forward-compat). Unknown values within a known
/// field (e.g. a non-existent theme name) still error, which is intentional —
/// silent typo tolerance for theme names would mask a real user problem.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct Config {
  pub theme: ThemeName,
  pub model_paths: Vec<PathBuf>,
  pub disable_default_cache_paths: CachePathsConfig,
  pub port_range: PortRange,
  pub llama_server_path: Option<PathBuf>,
  pub keybindings: BTreeMap<String, String>,
  pub disable_scan: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct CachePathsConfig {
  pub huggingface: bool,
  pub ollama: bool,
  pub lm_studio: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PortRange {
  pub start: u16,
  pub end: u16,
}

impl Default for PortRange {
  fn default() -> Self {
    // High, unprivileged, rarely claimed by common dev servers. Resolved
    // during planning (see plan Open Questions).
    Self {
      start: 41100,
      end: 41300,
    }
  }
}

/// Returned by `load_config_from_path`. `warning` is non-`None` when the
/// loader gracefully fell back to defaults but the user should be told why
/// (e.g. malformed YAML).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadedConfig {
  pub config: Config,
  pub warning: Option<String>,
}

/// Resolve which config file to load, given an optional env override and the
/// directory `directories` would pick. Pure function for testability —
/// mirrors `kdash::config::config_path_from`.
pub fn config_path_from(
  env_override: Option<OsString>,
  config_file: Option<PathBuf>,
) -> Option<PathBuf> {
  env_override
    .filter(|raw| !raw.is_empty())
    .map(PathBuf::from)
    .or(config_file)
}

/// Resolve the active config-file path using `$LLAMATUI_CONFIG` (if set)
/// and the OS-conventional location otherwise.
pub fn config_path() -> Option<PathBuf> {
  config_path_from(env::var_os("LLAMATUI_CONFIG"), user_config_file())
}

fn parse_config(contents: &str, path: &Path) -> LoadedConfig {
  match serde_yaml::from_str::<Config>(contents) {
    Ok(config) => LoadedConfig {
      config,
      warning: None,
    },
    Err(error) => LoadedConfig {
      config: Config::default(),
      warning: Some(format!(
        "failed to parse config file {}: {}. Using defaults.",
        path.display(),
        error
      )),
    },
  }
}

/// Load a YAML config from `path`. Missing files yield defaults with no
/// warning. Read or parse errors yield defaults with a warning so the caller
/// can surface them without aborting startup.
pub fn load_config_from_path(path: &Path) -> LoadedConfig {
  match fs::read_to_string(path) {
    Ok(contents) => parse_config(&contents, path),
    Err(error) if error.kind() == ErrorKind::NotFound => LoadedConfig::default(),
    Err(error) => LoadedConfig {
      config: Config::default(),
      warning: Some(format!(
        "failed to read config file {}: {}. Using defaults.",
        path.display(),
        error
      )),
    },
  }
}

/// Load the user's config from the conventional location. Warnings are
/// forwarded to the `warn!` log macro in addition to being returned.
pub fn load_config() -> LoadedConfig {
  let loaded = config_path()
    .map(|path| load_config_from_path(&path))
    .unwrap_or_default();
  if let Some(warning) = &loaded.warning {
    warn!("{warning}");
  }
  loaded
}

/// Validate that we have *some* place to look for models. If scanning is
/// disabled and no user-supplied paths exist, llamatui would start with an
/// empty list and no path forward — a confusing dead-end. Surface it
/// early.
pub fn validate_scan_settings(
  disable_scan: bool,
  cli_paths: &[PathBuf],
  env_paths: &[PathBuf],
  config_paths: &[PathBuf],
) -> Result<(), ScanSettingsError> {
  if disable_scan && cli_paths.is_empty() && env_paths.is_empty() && config_paths.is_empty() {
    Err(ScanSettingsError::NoScanWithoutPaths)
  } else {
    Ok(())
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanSettingsError {
  NoScanWithoutPaths,
}

impl std::fmt::Display for ScanSettingsError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::NoScanWithoutPaths => write!(
        f,
        "scanning is disabled but no model paths were supplied via --model-path, \
         LLAMATUI_MODEL_PATHS, or the `model_paths` config key — llamatui has nothing to list. \
         Provide at least one path or re-enable scanning."
      ),
    }
  }
}

impl std::error::Error for ScanSettingsError {}

#[cfg(test)]
mod tests {
  use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
  };

  use super::*;

  fn temp_test_dir(name: &str) -> PathBuf {
    let suffix = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .expect("system time should be after epoch")
      .as_nanos();
    let path = env::temp_dir().join(format!(
      "llamatui-config-tests-{}-{}-{}",
      name,
      std::process::id(),
      suffix
    ));
    fs::create_dir_all(&path).expect("temp test dir should be created");
    path
  }

  #[test]
  fn config_path_from_prefers_env_override() {
    let path = config_path_from(
      Some(OsString::from("/tmp/custom.yaml")),
      Some(PathBuf::from("/tmp/ignored.yaml")),
    );
    assert_eq!(path, Some(PathBuf::from("/tmp/custom.yaml")));
  }

  #[test]
  fn config_path_from_falls_back_to_xdg() {
    let path = config_path_from(
      None,
      Some(PathBuf::from("/home/u/.config/llamatui/config.yaml")),
    );
    assert_eq!(
      path,
      Some(PathBuf::from("/home/u/.config/llamatui/config.yaml"))
    );
  }

  #[test]
  fn config_path_from_ignores_empty_env_value() {
    let path = config_path_from(
      Some(OsString::new()),
      Some(PathBuf::from("/home/u/.config/llamatui/config.yaml")),
    );
    assert_eq!(
      path,
      Some(PathBuf::from("/home/u/.config/llamatui/config.yaml"))
    );
  }

  #[test]
  fn config_path_from_returns_none_when_both_sources_absent() {
    assert_eq!(config_path_from(None, None), None);
  }

  #[test]
  fn load_config_from_path_reads_valid_yaml() {
    let dir = temp_test_dir("valid");
    let path = dir.join("config.yaml");
    fs::write(
      &path,
      r"
theme: latte
disable_scan: false
model_paths:
  - /home/u/models
  - /mnt/storage/gguf
disable_default_cache_paths:
  ollama: true
port_range:
  start: 50000
  end: 50100
keybindings:
  quit: ctrl+q
",
    )
    .expect("config fixture should be written");

    let loaded = load_config_from_path(&path);

    assert!(loaded.warning.is_none(), "valid config should not warn");
    assert_eq!(loaded.config.theme, ThemeName::Latte);
    assert_eq!(
      loaded.config.model_paths,
      vec![
        PathBuf::from("/home/u/models"),
        PathBuf::from("/mnt/storage/gguf"),
      ]
    );
    assert!(loaded.config.disable_default_cache_paths.ollama);
    assert!(!loaded.config.disable_default_cache_paths.huggingface);
    assert!(!loaded.config.disable_default_cache_paths.lm_studio);
    assert_eq!(
      loaded.config.port_range,
      PortRange {
        start: 50000,
        end: 50100
      }
    );
    assert_eq!(
      loaded.config.keybindings.get("quit"),
      Some(&"ctrl+q".to_string())
    );

    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn load_config_from_path_missing_file_returns_defaults_silently() {
    let dir = temp_test_dir("missing");
    let path = dir.join("missing.yaml");
    let loaded = load_config_from_path(&path);

    assert_eq!(loaded.config, Config::default());
    assert!(loaded.warning.is_none());
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn load_config_from_path_malformed_yaml_uses_defaults_with_warning() {
    let dir = temp_test_dir("malformed");
    let path = dir.join("config.yaml");
    fs::write(&path, "theme: latte\nport_range: not-a-mapping").expect("write failed");

    let loaded = load_config_from_path(&path);

    assert_eq!(loaded.config, Config::default());
    let warning = loaded
      .warning
      .expect("malformed YAML must surface a warning");
    assert!(
      warning.contains("failed to parse config file"),
      "warning should name the failure: {warning}"
    );
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn load_config_from_path_unknown_theme_surfaces_warning() {
    let dir = temp_test_dir("unknown_theme");
    let path = dir.join("config.yaml");
    fs::write(&path, "theme: dracula\n").expect("write failed");

    let loaded = load_config_from_path(&path);

    assert_eq!(loaded.config, Config::default());
    let warning = loaded
      .warning
      .expect("unknown theme must surface a warning");
    assert!(
      warning.contains("dracula"),
      "warning should name the bad value: {warning}"
    );
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn load_config_from_path_partial_config_uses_defaults_for_unset_fields() {
    let dir = temp_test_dir("partial");
    let path = dir.join("config.yaml");
    fs::write(&path, "theme: gruvbox-dark\n").expect("write failed");

    let loaded = load_config_from_path(&path);

    assert!(loaded.warning.is_none());
    assert_eq!(loaded.config.theme, ThemeName::GruvboxDark);
    assert_eq!(loaded.config.port_range, PortRange::default());
    assert!(loaded.config.model_paths.is_empty());
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn default_config_uses_macchiato_and_default_port_range() {
    let cfg = Config::default();
    assert_eq!(cfg.theme, ThemeName::Macchiato);
    assert_eq!(
      cfg.port_range,
      PortRange {
        start: 41100,
        end: 41300
      }
    );
    assert!(!cfg.disable_scan);
  }

  #[test]
  fn validate_scan_settings_errors_when_disabled_with_no_paths() {
    let result = validate_scan_settings(true, &[], &[], &[]);
    assert_eq!(result, Err(ScanSettingsError::NoScanWithoutPaths));
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("scanning is disabled"), "{msg}");
    assert!(msg.contains("--model-path"), "{msg}");
  }

  #[test]
  fn validate_scan_settings_ok_when_paths_supplied_via_any_source() {
    assert!(validate_scan_settings(true, &[PathBuf::from("/a")], &[], &[]).is_ok());
    assert!(validate_scan_settings(true, &[], &[PathBuf::from("/b")], &[]).is_ok());
    assert!(validate_scan_settings(true, &[], &[], &[PathBuf::from("/c")]).is_ok());
  }

  #[test]
  fn validate_scan_settings_ok_when_scan_enabled() {
    assert!(validate_scan_settings(false, &[], &[], &[]).is_ok());
  }
}
