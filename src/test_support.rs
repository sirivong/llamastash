//! Shared helpers for integration tests under `tests/`.
//!
//! Gated behind the `test-fixtures` feature so consumer builds of the
//! library don't carry test-only utilities.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Unique temp directory for an integration test.
///
/// macOS `sun_path` is 104 bytes; the default `temp_dir()` already
/// eats ~50 of those, so we trim the time-based suffix and add a
/// process-local atomic counter. Two tests running on the same
/// millisecond used to share a directory (and a daemon, and a
/// runtime.json), which surfaced as periodic Connect-error flakes in
/// the chat smoke tests. `prefix` should be 2-5 chars.
pub fn unique_temp_dir(prefix: &str, label: &str) -> PathBuf {
  static SEQ: AtomicU64 = AtomicU64::new(0);
  let seq = SEQ.fetch_add(1, Ordering::Relaxed);
  let suffix = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .expect("clock")
    .as_millis()
    % 0xFFFF_FFFF;
  let dir = std::env::temp_dir().join(format!(
    "{prefix}-{label}-{}-{suffix:x}-{seq:x}",
    std::process::id()
  ));
  std::fs::create_dir_all(&dir).expect("temp dir creation");
  dir
}
