//! Process-wide log initialisation and panic hook.
//!
//! Logs are written to `cache_dir/logs/llamatui.log` in append mode.
//! Verbose mode bumps the default Info level up to Debug; further levels
//! (Trace, Warn, Error) are accessible via the env var `LLAMATUI_LOG`.

use std::{fs, fs::File, path::PathBuf, str::FromStr};

use anyhow::{Context, Result};
use log::LevelFilter;
use simplelog::{Config, WriteLogger};

use super::paths::log_dir;

/// Initialise the global logger. Returns the path of the log file that was
/// opened so the caller can surface it in error output.
pub fn init(verbose: bool) -> Result<PathBuf> {
  let level = resolve_level(verbose, std::env::var("LLAMATUI_LOG").ok().as_deref());
  let dir = log_dir().context("could not resolve a log directory for this platform")?;
  fs::create_dir_all(&dir)
    .with_context(|| format!("failed to create log directory at {}", dir.display()))?;
  let path = dir.join("llamatui.log");
  let file = File::options()
    .create(true)
    .append(true)
    .open(&path)
    .with_context(|| format!("failed to open log file at {}", path.display()))?;
  WriteLogger::init(level, Config::default(), file).context("logger already initialised")?;
  Ok(path)
}

fn resolve_level(verbose: bool, env: Option<&str>) -> LevelFilter {
  if let Some(raw) = env {
    if let Ok(level) = LevelFilter::from_str(raw) {
      return level;
    }
  }
  if verbose {
    LevelFilter::Debug
  } else {
    LevelFilter::Info
  }
}

/// Install a panic hook that records the panic to the log file and surfaces a
/// concise message on stderr. The terminal-restoration logic for the TUI
/// path lives in `tui::events` (added in Unit 6); this hook is the
/// always-installed baseline.
pub fn install_panic_hook() {
  std::panic::set_hook(Box::new(|info| {
    log::error!("panic: {}", info);
    eprintln!("\nllamatui panicked: {info}");
  }));
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn resolve_level_prefers_env_when_valid() {
    assert_eq!(resolve_level(false, Some("trace")), LevelFilter::Trace);
    assert_eq!(resolve_level(true, Some("warn")), LevelFilter::Warn);
  }

  #[test]
  fn resolve_level_ignores_invalid_env() {
    assert_eq!(resolve_level(false, Some("noisy")), LevelFilter::Info);
    assert_eq!(resolve_level(true, Some("verybad")), LevelFilter::Debug);
  }

  #[test]
  fn resolve_level_defaults_to_info_or_debug_with_verbose() {
    assert_eq!(resolve_level(false, None), LevelFilter::Info);
    assert_eq!(resolve_level(true, None), LevelFilter::Debug);
  }
}
