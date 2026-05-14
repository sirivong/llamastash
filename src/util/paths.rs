//! OS-aware path resolution for state, config, cache, logs, and the runtime
//! socket.
//!
//! Linux follows XDG; macOS follows the Apple convention (everything under
//! `~/Library/...`). `state_dir` falls back to `data_dir` on macOS because
//! the `directories` crate only exposes a distinct state directory on Linux.
//! The runtime socket falls back to `$TMPDIR/llamatui-$USER/daemon.sock`
//! when no `runtime_dir` is available.

use std::{ffi::OsString, path::PathBuf};

use directories::ProjectDirs;

const QUALIFIER: &str = "";
const ORGANIZATION: &str = "";
const APPLICATION: &str = "llamatui";

pub fn project_dirs() -> Option<ProjectDirs> {
  ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
}

pub fn state_dir() -> Option<PathBuf> {
  project_dirs().map(|d| {
    d.state_dir()
      .map_or_else(|| d.data_dir().to_path_buf(), PathBuf::from)
  })
}

pub fn config_dir() -> Option<PathBuf> {
  project_dirs().map(|d| d.config_dir().to_path_buf())
}

pub fn cache_dir() -> Option<PathBuf> {
  project_dirs().map(|d| d.cache_dir().to_path_buf())
}

pub fn log_dir() -> Option<PathBuf> {
  cache_dir().map(|d| d.join("logs"))
}

/// Resolve the Unix-socket path for the daemon.
///
/// Always returns a path. `XDG_RUNTIME_DIR` is preferred (Linux); macOS and
/// environments without it fall back to `$TMPDIR/llamatui-$USER/daemon.sock`.
pub fn runtime_socket_path() -> PathBuf {
  if let Some(dirs) = project_dirs() {
    if let Some(rt) = dirs.runtime_dir() {
      return rt.join("daemon.sock");
    }
  }
  fallback_runtime_dir_from(std::env::var_os("TMPDIR"), &username()).join("daemon.sock")
}

fn username() -> String {
  std::env::var("USER")
    .or_else(|_| std::env::var("LOGNAME"))
    .unwrap_or_else(|_| String::from("default"))
}

/// Build the fallback runtime directory used when `XDG_RUNTIME_DIR` is not
/// set (notably on macOS). Per-user scoped so concurrent users on the same
/// host don't collide.
fn fallback_runtime_dir_from(tmpdir: Option<OsString>, user: &str) -> PathBuf {
  let tmp = tmpdir
    .map(PathBuf::from)
    .unwrap_or_else(|| PathBuf::from("/tmp"));
  tmp.join(format!("llamatui-{user}"))
}

/// Convenience: return the canonical user config-file path.
pub fn user_config_file() -> Option<PathBuf> {
  config_dir().map(|d| d.join("config.yaml"))
}

/// Convenience: state-store path (favorites, presets, last-params, running snapshot).
pub fn state_file() -> Option<PathBuf> {
  state_dir().map(|d| d.join("state.json"))
}

/// Convenience: PID lockfile path used by the daemon to enforce single-instance.
pub fn daemon_pidfile() -> Option<PathBuf> {
  state_dir().map(|d| d.join("daemon.pid"))
}

#[cfg(test)]
mod tests {
  use std::path::Path;

  use super::*;

  /// Pure helper used in tests so callers can verify the path-joining logic
  /// without manipulating process-wide environment variables.
  fn build_socket_path(runtime_dir: Option<&Path>, fallback_tmp: &Path, user: &str) -> PathBuf {
    if let Some(rt) = runtime_dir {
      return rt.join("daemon.sock");
    }
    fallback_tmp
      .join(format!("llamatui-{user}"))
      .join("daemon.sock")
  }

  #[test]
  fn project_dirs_resolves_on_this_platform() {
    // Smoke test: directories crate must produce a project root on every
    // platform we support. If this returns None the test environment has no
    // resolvable home directory, which is a broader problem than we want to
    // hide here.
    assert!(
      project_dirs().is_some(),
      "ProjectDirs::from should resolve on a normal developer machine"
    );
  }

  #[test]
  fn all_dir_helpers_return_some() {
    assert!(state_dir().is_some());
    assert!(config_dir().is_some());
    assert!(cache_dir().is_some());
    assert!(log_dir().is_some());
    assert!(user_config_file().is_some());
    assert!(state_file().is_some());
    assert!(daemon_pidfile().is_some());
  }

  #[test]
  fn runtime_socket_path_always_resolves() {
    // Infallible by design — see runtime_socket_path's contract.
    let path = runtime_socket_path();
    assert_eq!(
      path.file_name().and_then(|s| s.to_str()),
      Some("daemon.sock")
    );
  }

  #[test]
  fn log_dir_is_logs_under_cache_dir() {
    let log = log_dir().unwrap();
    let cache = cache_dir().unwrap();
    assert_eq!(log, cache.join("logs"));
  }

  #[test]
  fn user_config_file_lives_under_config_dir() {
    let path = user_config_file().unwrap();
    assert_eq!(path, config_dir().unwrap().join("config.yaml"));
  }

  #[test]
  fn state_file_lives_under_state_dir() {
    let path = state_file().unwrap();
    assert_eq!(path, state_dir().unwrap().join("state.json"));
  }

  #[test]
  fn daemon_pidfile_lives_under_state_dir() {
    let path = daemon_pidfile().unwrap();
    assert_eq!(path, state_dir().unwrap().join("daemon.pid"));
  }

  #[test]
  fn build_socket_path_uses_runtime_dir_when_present() {
    let rt = PathBuf::from("/run/user/1000/llamatui");
    let path = build_socket_path(Some(&rt), Path::new("/tmp"), "ignored");
    assert_eq!(path, PathBuf::from("/run/user/1000/llamatui/daemon.sock"));
  }

  #[test]
  fn build_socket_path_falls_back_to_tmp_dir_with_username() {
    let path = build_socket_path(None, Path::new("/tmp"), "alice");
    assert_eq!(path, PathBuf::from("/tmp/llamatui-alice/daemon.sock"));
  }

  #[test]
  fn fallback_runtime_dir_uses_provided_tmp_and_username() {
    let path = fallback_runtime_dir_from(Some(OsString::from("/var/tmp")), "bob");
    assert_eq!(path, PathBuf::from("/var/tmp/llamatui-bob"));
  }

  #[test]
  fn fallback_runtime_dir_defaults_to_slash_tmp() {
    let path = fallback_runtime_dir_from(None, "default");
    assert_eq!(path, PathBuf::from("/tmp/llamatui-default"));
  }
}
