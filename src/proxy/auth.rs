//! Bearer-token auth for the LAN-exposed proxy data plane.
//!
//! Mirrors the control-plane's `IpcToken` story (`src/daemon/auth.rs`):
//! a high-entropy key, constant-time comparison, and a redacted
//! `Debug` so the secret never lands in a log line. The two differ in
//! lifetime — the control-plane token is regenerated every daemon
//! start and lives in `runtime.json`, whereas the proxy key is durable
//! (clients hard-code it) and lives in `config.yaml`.
//!
//! Auth is enforced only when a key is configured. The default
//! loopback, keyless posture skips the check before touching the
//! request headers ([`ProxyAuth::enforced`] is a single bool), so the
//! benchmarked hot path is unchanged.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hyper::header::{HeaderMap, AUTHORIZATION};
use rand::TryRngCore;

use crate::daemon::auth::{constant_time_eq, extract_bearer};

/// Raw entropy bytes behind a generated key (before encoding). 32
/// bytes is ~256 bits — the same bar as the control-plane token.
const KEY_BYTES: usize = 32;

/// Prefix on generated keys so they read as OpenAI-style `sk-` keys in
/// client configs and are greppable in a user's shell history.
const KEY_PREFIX: &str = "sk-llamastash-";

/// A proxy bearer key. Wraps the encoded string so equality is
/// constant-time and an accidental `Debug` never prints the secret.
#[derive(Clone)]
pub struct ProxyApiKey(String);

impl ProxyApiKey {
  /// Generate a fresh key: `sk-llamastash-<43-char base64url>`.
  /// Panics only if the OS randomness source is unavailable — the same
  /// non-recoverable state `IpcToken::generate` treats as fatal.
  pub fn generate() -> Self {
    let mut bytes = [0u8; KEY_BYTES];
    rand::rngs::OsRng
      .try_fill_bytes(&mut bytes)
      .expect("OsRng must succeed for proxy key generation");
    Self(format!("{KEY_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes)))
  }

  /// Wrap an existing key string (config / env path).
  pub fn from_string(raw: String) -> Self {
    Self(raw)
  }

  /// Borrow the full secret for persistence / one-time printing. The
  /// returned slice is the secret; callers must not log it.
  pub fn as_str(&self) -> &str {
    &self.0
  }

  /// Constant-time comparison against a candidate token.
  pub fn verify(&self, candidate: &str) -> bool {
    constant_time_eq(self.0.as_bytes(), candidate.as_bytes())
  }
}

impl std::fmt::Debug for ProxyApiKey {
  // Suppress the secret in any Debug output.
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ProxyApiKey")
      .field("len", &self.0.len())
      .finish()
  }
}

/// Per-listener auth policy. `None` key = auth disabled (loopback,
/// same-UID posture). `Some` = a `Bearer` token is required on the
/// data routes.
#[derive(Clone, Debug, Default)]
pub struct ProxyAuth {
  key: Option<ProxyApiKey>,
}

impl ProxyAuth {
  /// Build from the resolved config key. `None` or a blank string
  /// disables auth.
  pub fn new(key: Option<String>) -> Self {
    Self {
      key: key
        .filter(|k| !k.trim().is_empty())
        .map(ProxyApiKey::from_string),
    }
  }

  /// Whether auth is enforced (a key is configured). Cheap — a single
  /// `Option::is_some`. Callers gate on this before doing any header
  /// work so the keyless path stays free.
  pub fn enforced(&self) -> bool {
    self.key.is_some()
  }

  /// Whether the request is authorized: `true` when auth is disabled
  /// or the `Authorization: Bearer <key>` matches the configured key;
  /// `false` on a missing / malformed / wrong token.
  pub fn check(&self, headers: &HeaderMap) -> bool {
    let Some(key) = &self.key else {
      return true;
    };
    headers
      .get(AUTHORIZATION)
      .and_then(|v| v.to_str().ok())
      .and_then(extract_bearer)
      .is_some_and(|tok| key.verify(tok))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use hyper::header::HeaderValue;

  fn headers_with(auth: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(AUTHORIZATION, HeaderValue::from_str(auth).unwrap());
    h
  }

  #[test]
  fn generated_key_has_prefix_and_is_unique() {
    let a = ProxyApiKey::generate();
    let b = ProxyApiKey::generate();
    assert!(a.as_str().starts_with(KEY_PREFIX));
    // prefix + 43 base64url chars (ceil(32 * 8 / 6) = 43).
    assert_eq!(a.as_str().len(), KEY_PREFIX.len() + 43);
    assert_ne!(a.as_str(), b.as_str(), "two fresh keys collided");
  }

  #[test]
  fn key_verifies_self_and_rejects_others() {
    let k = ProxyApiKey::generate();
    assert!(k.verify(k.as_str()));
    assert!(!k.verify("sk-llamastash-wrong"));
    assert!(!k.verify(""));
  }

  #[test]
  fn debug_redacts_the_secret() {
    let k = ProxyApiKey::from_string("sk-llamastash-supersecret".into());
    let shown = format!("{k:?}");
    assert!(
      !shown.contains("supersecret"),
      "Debug leaked the key: {shown}"
    );
  }

  #[test]
  fn disabled_auth_allows_everything() {
    let auth = ProxyAuth::new(None);
    assert!(!auth.enforced());
    assert!(auth.check(&HeaderMap::new()));
    // A blank key string is treated as no key.
    assert!(!ProxyAuth::new(Some("   ".into())).enforced());
  }

  #[test]
  fn enabled_auth_requires_matching_bearer() {
    let auth = ProxyAuth::new(Some("sk-llamastash-k3y".into()));
    assert!(auth.enforced());
    // Correct token passes.
    assert!(auth.check(&headers_with("Bearer sk-llamastash-k3y")));
    // Wrong token, missing header, and non-Bearer scheme all reject.
    assert!(!auth.check(&headers_with("Bearer nope")));
    assert!(!auth.check(&HeaderMap::new()));
    assert!(!auth.check(&headers_with("Basic sk-llamastash-k3y")));
  }
}
