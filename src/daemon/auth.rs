//! Bearer-token authentication for the control-plane HTTP listener.
//!
//! The token is generated fresh on every daemon start (32 bytes from
//! `OsRng`, base64url-encoded without padding) and written to
//! `runtime.json` alongside the resolved listener URL. Clients read
//! the file (or honor `LLAMASTASH_IPC_TOKEN`) and present the token in
//! an `Authorization: Bearer <token>` header on every request except
//! `/health`. Token comparison is constant-time.
//!
//! The token plus filesystem permissions on `runtime.json` (0o600 on
//! Unix, DACL-restricted on Windows) is the entire control-plane auth
//! story — the kernel-attested same-UID assumption from the previous
//! `SO_PEERCRED` design carries over via the file's permission mode.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::TryRngCore;

/// Length of the raw token bytes before base64url encoding. 32 bytes
/// of OS-randomness gives ~256 bits of entropy — well past the bar
/// for a same-machine secret rotated per daemon start.
pub const TOKEN_BYTES: usize = 32;

/// Per-daemon bearer token. Wraps the base64url-encoded string so
/// equality compares are constant-time and accidental `Debug` logs
/// don't leak the secret.
#[derive(Clone)]
pub struct IpcToken(String);

impl IpcToken {
  /// Generate a fresh token from `OsRng`. Panics only if the OS
  /// randomness source is unavailable, which on Linux/macOS means
  /// `getrandom(2)` returned an error — a non-recoverable system
  /// state where a panic is the honest response.
  pub fn generate() -> Self {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::rngs::OsRng
      .try_fill_bytes(&mut bytes)
      .expect("OsRng must succeed for daemon startup");
    Self(URL_SAFE_NO_PAD.encode(bytes))
  }

  /// Wrap an existing token string (env override path / tests).
  pub fn from_string(raw: String) -> Self {
    Self(raw)
  }

  /// Borrow the encoded string for transport / serialization. The
  /// returned slice contains the full secret; callers must not log
  /// it.
  pub fn as_str(&self) -> &str {
    &self.0
  }

  /// Consume the token to recover the owned string. Same secrecy
  /// caveat as `as_str`.
  pub fn into_string(self) -> String {
    self.0
  }

  /// Constant-time comparison against a candidate string. Returns
  /// `true` iff the two byte sequences are byte-identical. Early
  /// length mismatch is acceptable — leaking the token length is not
  /// a useful signal to an attacker.
  pub fn verify(&self, candidate: &str) -> bool {
    constant_time_eq(self.0.as_bytes(), candidate.as_bytes())
  }
}

impl std::fmt::Debug for IpcToken {
  // Suppress the secret in any Debug output; downstream `log::debug!`
  // / `format!` calls that wrap the token never accidentally emit it.
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("IpcToken")
      .field("len", &self.0.len())
      .finish()
  }
}

/// Extract the bearer token value from an `Authorization` header
/// value, or `None` if the header is missing the `Bearer ` prefix /
/// has trailing junk. Case-sensitive on the scheme per RFC 6750 §2.1
/// (servers MAY be case-insensitive, but we don't need to be — every
/// client we ship sends `Bearer ` verbatim).
pub fn extract_bearer(header_value: &str) -> Option<&str> {
  header_value.strip_prefix("Bearer ").map(str::trim)
}

/// Constant-time byte slice comparison. Length-aware early-exit is
/// deliberate (the slot is the secret length, not the secret itself
/// — leaking it via timing or a fast path is acceptable). Shared with
/// the proxy's bearer key (`crate::proxy::auth::ProxyApiKey`).
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
  if a.len() != b.len() {
    return false;
  }
  let mut diff: u8 = 0;
  for (x, y) in a.iter().zip(b.iter()) {
    diff |= x ^ y;
  }
  diff == 0
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn generate_produces_unique_tokens() {
    let a = IpcToken::generate();
    let b = IpcToken::generate();
    assert_ne!(a.as_str(), b.as_str(), "two fresh tokens collided");
    // 32 bytes base64url-encoded without padding lands at 43 chars
    // (ceil(32 * 4 / 3) = 43; the standard formula is ceil(N * 8 / 6)).
    assert_eq!(a.as_str().len(), 43);
  }

  #[test]
  fn verify_accepts_self() {
    let t = IpcToken::generate();
    let candidate = t.as_str().to_owned();
    assert!(t.verify(&candidate));
  }

  #[test]
  fn verify_rejects_wrong_token() {
    let t = IpcToken::generate();
    assert!(!t.verify("not-the-token"));
    assert!(!t.verify(""));
  }

  #[test]
  fn verify_rejects_length_mismatch() {
    let t = IpcToken::from_string("short".into());
    assert!(!t.verify("shorter"));
    assert!(!t.verify("much-longer-than-the-token"));
  }

  #[test]
  fn extract_bearer_strips_prefix() {
    assert_eq!(extract_bearer("Bearer abc"), Some("abc"));
    assert_eq!(extract_bearer("Bearer  spaced  "), Some("spaced"));
  }

  #[test]
  fn extract_bearer_rejects_non_bearer() {
    assert_eq!(extract_bearer(""), None);
    assert_eq!(extract_bearer("Basic abc"), None);
    assert_eq!(extract_bearer("bearer abc"), None); // case-sensitive on scheme
  }

  #[test]
  fn debug_does_not_leak_secret() {
    let t = IpcToken::from_string("super-secret-token-value".into());
    let dbg = format!("{t:?}");
    assert!(!dbg.contains("super-secret-token-value"));
    assert!(dbg.contains("IpcToken"));
  }
}
