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

/// Best-effort **synchronous** daemon shutdown, for test `Drop` guards (Drop
/// runs during unwind and can't drive an async client). Hand-rolls an
/// HTTP/1.0 `POST /rpc` carrying the JSON-RPC `shutdown` envelope against the
/// URL + token recorded in `runtime.json`. That trips the daemon's shutdown
/// token, so `run_foreground` runs its `stop_all_managed` step — which is
/// where every `setsid`-detached supervised child (`fake_llama_server`) gets
/// SIGTERM/SIGKILLed. Without it those children become init-owned orphans, the
/// historical source of leaked test fixtures. No-op when `runtime.json` is
/// absent (daemon already gone).
pub fn sync_shutdown_daemon(state_dir: &std::path::Path) -> std::io::Result<()> {
  use std::io::{Read, Write};
  use std::net::TcpStream;
  use std::time::Duration;
  let info = match crate::daemon::runtime_file::load(state_dir) {
    Ok(Some(i)) => i,
    _ => return Ok(()),
  };
  // The daemon binds loopback only, so the URL is always `http://127.0.0.1:<port>`.
  let host_port = info
    .ipc_url
    .strip_prefix("http://")
    .unwrap_or(info.ipc_url.as_str());
  let mut stream = TcpStream::connect(host_port)?;
  stream.set_write_timeout(Some(Duration::from_secs(1)))?;
  stream.set_read_timeout(Some(Duration::from_secs(1)))?;
  let body = br#"{"jsonrpc":"2.0","id":1,"method":"shutdown"}"#;
  let req = format!(
    "POST /rpc HTTP/1.0\r\n\
     Host: {host_port}\r\n\
     Authorization: Bearer {token}\r\n\
     Content-Type: application/json\r\n\
     Content-Length: {len}\r\n\
     Connection: close\r\n\r\n",
    token = info.ipc_token,
    len = body.len(),
  );
  stream.write_all(req.as_bytes())?;
  stream.write_all(body)?;
  // Drain the response so the daemon's writer doesn't block on a full peer
  // buffer; the content doesn't matter — only that the token was tripped.
  let mut sink = [0u8; 512];
  let _ = stream.read(&mut sink);
  Ok(())
}
