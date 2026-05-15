//! Cross-platform clipboard write helper used by the TUI's yank
//! commands (R30).
//!
//! v1 strategy: shell out to whichever of `wl-copy`, `xclip`,
//! `xsel`, or `pbcopy` (macOS) is available, in that priority. We
//! deliberately avoid `arboard` for v1 to keep the build dep-light;
//! the shell-out fallback also doesn't require an X/Wayland display
//! at compile time. If every tool is missing the call returns
//! `ClipboardError::NoBackend`; the TUI surfaces a transient toast
//! that prints the URL inline so the user can copy by hand.

use std::io::Write;
use std::process::{Command, Stdio};

/// Outcome of a clipboard write.
#[derive(Debug)]
pub enum ClipboardError {
  /// None of the supported helper binaries are on `$PATH`.
  NoBackend { tried: Vec<&'static str> },
  /// The backend was found but failed (non-zero exit / IO error).
  /// `backend` names which one was attempted.
  BackendFailed {
    backend: &'static str,
    error: String,
  },
}

impl std::fmt::Display for ClipboardError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::NoBackend { tried } => write!(
        f,
        "no clipboard backend available — tried: {}. Copy manually.",
        tried.join(", ")
      ),
      Self::BackendFailed { backend, error } => {
        write!(f, "{backend} clipboard write failed: {error}")
      }
    }
  }
}

impl std::error::Error for ClipboardError {}

/// Copy `text` into the system clipboard. Returns the name of the
/// backend that succeeded so the caller can mention it in a toast.
pub fn write(text: &str) -> Result<&'static str, ClipboardError> {
  write_with(text, &default_backends())
}

/// Backend candidate list in priority order. Wayland-first because
/// modern Linux desktops are usually Wayland; X tools work in
/// XWayland too but `wl-copy` is the native path. `pbcopy` only
/// matters on macOS.
fn default_backends() -> Vec<&'static str> {
  if cfg!(target_os = "macos") {
    vec!["pbcopy"]
  } else {
    vec!["wl-copy", "xclip", "xsel"]
  }
}

fn write_with(text: &str, backends: &[&'static str]) -> Result<&'static str, ClipboardError> {
  let mut tried: Vec<&'static str> = Vec::new();
  for backend in backends {
    if which_on_path(backend).is_none() {
      tried.push(backend);
      continue;
    }
    match write_via(backend, text) {
      Ok(()) => return Ok(backend),
      Err(e) => {
        return Err(ClipboardError::BackendFailed {
          backend,
          error: e.to_string(),
        })
      }
    }
  }
  Err(ClipboardError::NoBackend { tried })
}

fn write_via(backend: &str, text: &str) -> std::io::Result<()> {
  let args: &[&str] = match backend {
    // `xclip` defaults to PRIMARY; `-selection clipboard` puts the
    // text where Ctrl+V pastes. `xsel`'s `--input --clipboard` does
    // the same.
    "xclip" => &["-selection", "clipboard"],
    "xsel" => &["--input", "--clipboard"],
    _ => &[],
  };
  let mut child = Command::new(backend)
    .args(args)
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()?;
  if let Some(mut stdin) = child.stdin.take() {
    stdin.write_all(text.as_bytes())?;
  }
  let output = child.wait_with_output()?;
  if !output.status.success() {
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    return Err(std::io::Error::other(format!(
      "{backend} exited {:?}: {stderr}",
      output.status.code()
    )));
  }
  Ok(())
}

/// `which`-style check that doesn't pull in the `which` crate's
/// canonicalisation overhead. Splits `$PATH` and probes for the
/// first executable hit.
fn which_on_path(bin: &str) -> Option<std::path::PathBuf> {
  let path = std::env::var_os("PATH")?;
  for dir in std::env::split_paths(&path) {
    let candidate = dir.join(bin);
    if is_executable_file(&candidate) {
      return Some(candidate);
    }
  }
  None
}

#[cfg(unix)]
fn is_executable_file(path: &std::path::Path) -> bool {
  use std::os::unix::fs::PermissionsExt;
  match std::fs::metadata(path) {
    Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
    Err(_) => false,
  }
}

#[cfg(not(unix))]
fn is_executable_file(path: &std::path::Path) -> bool {
  path.is_file()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn no_backend_error_lists_what_was_tried() {
    let err = write_with("hello", &["nonexistent-clipboard-binary-9f3a"]).unwrap_err();
    match err {
      ClipboardError::NoBackend { tried } => {
        assert_eq!(tried, vec!["nonexistent-clipboard-binary-9f3a"]);
      }
      other => panic!("expected NoBackend, got {other:?}"),
    }
  }

  #[test]
  fn no_backend_message_is_actionable() {
    let err = ClipboardError::NoBackend {
      tried: vec!["wl-copy", "xclip"],
    };
    let msg = err.to_string();
    assert!(msg.contains("Copy manually"));
    assert!(msg.contains("wl-copy"));
    assert!(msg.contains("xclip"));
  }

  #[test]
  fn default_backends_pick_correct_set_per_platform() {
    let backends = default_backends();
    if cfg!(target_os = "macos") {
      assert_eq!(backends, vec!["pbcopy"]);
    } else {
      assert!(backends.contains(&"wl-copy"));
      assert!(backends.contains(&"xclip"));
      assert!(backends.contains(&"xsel"));
    }
  }

  #[test]
  fn write_with_uses_first_backend_that_exists() {
    // `cat` is on PATH on every dev box and copies stdin to
    // stdout. We don't actually verify the clipboard contents (the
    // test box has no real clipboard); we just check `write_with`
    // returns success when *some* backend works.
    let backend = write_with("hello", &["cat"]).expect("cat backend works");
    assert_eq!(backend, "cat");
  }
}
