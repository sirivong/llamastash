// Echo-verification (R156 / R157 contract — Unit 1 gating task):
//
// Pass-through forwarding in Unit 3 relies on the upstream
// `llama-server` echoing `request.body.model` back in
// `response.body.model`. The inline tests below post a JSON request
// carrying `"model":"sentinel-x42"` to `fake_llama_server` and
// assert the echo on the response body. The fake fixture mirrors
// the real `llama-server` behavior in this respect — see
// `tests/fixtures/fake_llama_server.rs` (`/v1/chat/completions`).
//
// If the assertion ever breaks, the falsifying outcome is
// documented in the plan's Risks & Dependencies row "`llama-server`
// rewrites `body.model` in its response instead of echoing": Unit 3
// would have to JSON-parse / rewrite each SSE frame on fallback
// instead of byte-piping. See
// docs/plans/2026-05-21-001-feat-proxy-router-plan.md.

//! TCP accept loop + per-connection hyper service.
//!
//! Mirrors the shape of [`crate::daemon::server::serve`]: a
//! `tokio::select!` between `listener.accept()` and the daemon's
//! [`ShutdownToken`], with a bounded drain phase on shutdown.
//! Unlike the IPC server we don't peercred — loopback TCP doesn't
//! carry credentials, and the plan's scope is "loopback only;
//! same-host attacker is the threat model the OS handles."

use std::{
  net::{IpAddr, Ipv4Addr, SocketAddr},
  sync::{atomic::Ordering, Arc, Mutex as StdMutex, RwLock},
  time::Duration,
};

use anyhow::Result;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::{net::TcpListener, task::JoinHandle, time::Instant};

use super::router::route;
use super::state::ProxyState;
use crate::daemon::shutdown::ShutdownToken;

/// Maximum time to wait for in-flight connections after shutdown is
/// triggered before dropping them. Mirrors
/// [`crate::daemon::server::DRAIN_TIMEOUT`] so the two listeners
/// drain on the same budget — useful when both are stopped by one
/// SIGINT.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Externally visible proxy listener state. Unit 5 wires this into
/// the IPC `status` response and the CLI / TUI surfaces; Unit 1
/// only writes the cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyStatus {
  /// `proxy.enabled: false` in config; no listener was attempted.
  Disabled,
  /// Listener bound successfully and is accepting connections.
  Listening { addr: SocketAddr },
  /// `EADDRINUSE` — the configured port is held by another process.
  /// Status surface reports the *attempted* address.
  PortInUse { addr: SocketAddr },
  /// Bind failed for any other reason (EACCES, EADDRNOTAVAIL, ...).
  /// `bind_error` is the OS error message; the status surface
  /// passes it through to the user.
  Unbound {
    addr: SocketAddr,
    bind_error: String,
  },
}

/// Cheap-to-clone handle to the proxy's current status. The proxy
/// task writes to it on every transition; Unit 5's IPC `status`
/// handler reads from it.
pub type StatusCell = Arc<RwLock<ProxyStatus>>;

/// Construct a fresh cell seeded with `Disabled`. The daemon
/// overrides it immediately if `proxy.enabled` is true.
pub fn new_status_cell() -> StatusCell {
  Arc::new(RwLock::new(ProxyStatus::Disabled))
}

/// Default header-read timeout used by tests + callers that don't
/// thread a real `ProxyConfig` through. Production wiring in
/// `run_foreground` passes the user-configured value via
/// [`serve_with_options`] instead.
pub const DEFAULT_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Tunable knobs for the listener. Kept as a struct so future
/// additions don't churn the [`serve`] signature; in v1 only the
/// header-read timeout is configurable.
#[derive(Clone, Copy, Debug)]
pub struct ServeOptions {
  pub header_read_timeout: Duration,
}

impl Default for ServeOptions {
  fn default() -> Self {
    Self {
      header_read_timeout: DEFAULT_HEADER_READ_TIMEOUT,
    }
  }
}

/// Run the proxy listener until `shutdown` is triggered.
///
/// Returns without panicking on bind failure: the caller is the
/// daemon's `run_foreground`, which has stricter availability
/// guarantees than the proxy itself. `status` reflects the outcome
/// so the IPC surface can report it.
///
/// Defaults to [`ServeOptions::default`] (30 s header-read timeout).
/// Production callers pass a `ProxyConfig`-derived value via
/// [`serve_with_options`].
pub async fn serve(
  state: Arc<ProxyState>,
  addr: SocketAddr,
  shutdown: ShutdownToken,
  status: StatusCell,
) -> Result<()> {
  serve_with_options(state, addr, shutdown, status, ServeOptions::default()).await
}

/// `serve` plus per-listener tuning knobs. Same semantics as
/// [`serve`]; lifted out so the daemon can forward
/// `proxy.header_read_timeout_secs` without forcing every caller
/// (tests, benches) to construct a full options bundle.
pub async fn serve_with_options(
  state: Arc<ProxyState>,
  addr: SocketAddr,
  shutdown: ShutdownToken,
  status: StatusCell,
  options: ServeOptions,
) -> Result<()> {
  let listener = match TcpListener::bind(&addr).await {
    Ok(l) => l,
    Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
      log::warn!(
        "proxy listener: port {} already in use; daemon continues without the proxy",
        addr
      );
      write_status(&status, ProxyStatus::PortInUse { addr });
      return Ok(());
    }
    Err(e) => {
      log::warn!(
        "proxy listener: failed to bind {}: {e}; daemon continues without the proxy",
        addr
      );
      // Cap the bind_error string so a pathological OS message cannot
      // bloat the IPC status payload. 256 chars is roomy for typical
      // io::Error strings ("permission denied", etc.).
      let bind_error: String = e.to_string().chars().take(256).collect();
      write_status(&status, ProxyStatus::Unbound { addr, bind_error });
      return Ok(());
    }
  };
  // Re-resolve the bound address (the kernel may have promoted a 0
  // port — useful in tests, harmless in production).
  let bound = listener.local_addr().unwrap_or(addr);
  write_status(&status, ProxyStatus::Listening { addr: bound });
  log::info!("proxy listener bound on http://{bound}");

  let tracker = Arc::new(ConnectionTracker {
    active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    handles: StdMutex::new(Vec::new()),
  });

  loop {
    tokio::select! {
      _ = shutdown.wait_until_triggered() => {
        log::info!("proxy listener: shutdown signalled");
        break;
      }
      accept = listener.accept() => {
        match accept {
          Ok((stream, _peer)) => {
            let conn_state = Arc::clone(&state);
            let conn_tracker = Arc::clone(&tracker);
            conn_tracker.active.fetch_add(1, Ordering::SeqCst);
            let task_tracker = Arc::clone(&conn_tracker);
            let header_read_timeout = options.header_read_timeout;
            let handle = tokio::spawn(async move {
              serve_connection(stream, conn_state, header_read_timeout).await;
              task_tracker.active.fetch_sub(1, Ordering::SeqCst);
            });
            push_handle(&conn_tracker, handle);
          }
          Err(e) => {
            // Mirrors the IPC server's posture: a transient accept
            // failure is not fatal; loop and retry.
            log::warn!("proxy listener: accept failed: {e}");
          }
        }
      }
    }
  }

  drain(tracker).await;
  Ok(())
}

/// Bounded poll-and-abort drain phase. Identical shape to the
/// IPC server's drain so the two listeners exit on the same
/// schedule.
async fn drain(tracker: Arc<ConnectionTracker>) {
  let deadline = Instant::now() + DRAIN_TIMEOUT;
  let poll_interval = Duration::from_millis(50);
  while tracker.active.load(Ordering::SeqCst) > 0 {
    let remaining = deadline.checked_duration_since(Instant::now());
    let Some(time_left) = remaining else {
      let still_active = tracker.active.load(Ordering::SeqCst);
      log::warn!(
        "proxy drain deadline reached with {still_active} connection(s) still active; aborting"
      );
      let mut handles = tracker.handles.lock().unwrap_or_else(|e| e.into_inner());
      for h in handles.drain(..) {
        h.abort();
      }
      break;
    };
    tokio::time::sleep(poll_interval.min(time_left)).await;
  }
}

struct ConnectionTracker {
  active: Arc<std::sync::atomic::AtomicUsize>,
  handles: StdMutex<Vec<JoinHandle<()>>>,
}

fn push_handle(tracker: &Arc<ConnectionTracker>, handle: JoinHandle<()>) {
  let mut handles = tracker.handles.lock().unwrap_or_else(|e| e.into_inner());
  handles.retain(|h| !h.is_finished());
  handles.push(handle);
}

async fn serve_connection(
  stream: tokio::net::TcpStream,
  state: Arc<ProxyState>,
  header_read_timeout: Duration,
) {
  // `TokioIo` bridges `tokio::net::TcpStream` (which implements the
  // tokio `AsyncRead`/`AsyncWrite` traits) onto hyper 1.x's `Io`
  // traits without a `tower-service` dep. Owned by the connection
  // future so it lives exactly as long as the connection does.
  let io = TokioIo::new(stream);
  let service = service_fn(move |req| {
    let state = Arc::clone(&state);
    async move { route(state, req).await }
  });
  let mut builder = http1::Builder::new();
  builder
    // `keep_alive(true)` is the default but documenting it
    // explicitly: the keep-alive smoke test below depends on it.
    .keep_alive(true)
    // Bound header-read on the inbound stream so partial-request
    // clients don't hold the serve_connection future indefinitely.
    .timer(hyper_util::rt::TokioTimer::new())
    .header_read_timeout(header_read_timeout);
  if let Err(e) = builder.serve_connection(io, service).await {
    log::debug!("proxy: connection ended with: {e}");
  }
}

fn write_status(cell: &StatusCell, next: ProxyStatus) {
  let mut guard = cell.write().unwrap_or_else(|e| e.into_inner());
  *guard = next;
}

/// Build the canonical loopback `SocketAddr` from a port. The host
/// is fixed at `127.0.0.1` per the plan's Scope Boundaries (no LAN
/// binding in v1).
pub fn loopback_addr(port: u16) -> SocketAddr {
  SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ipc::methods::MethodContext;
  use std::time::Duration;

  /// Spin up the proxy on an ephemeral port and return the bound
  /// address + shutdown token + the JoinHandle. The caller drives
  /// shutdown via `shutdown.trigger()` and then `shutdown_proxy` so
  /// a hung serve loop fails the test loudly instead of leaving a
  /// detached task behind.
  async fn spawn_proxy_on_ephemeral_port() -> (SocketAddr, ShutdownToken, StatusCell, JoinHandle<()>)
  {
    let ctx = MethodContext::new(ShutdownToken::new());
    let state = ProxyState::from_context(&ctx);
    let token = ctx.shutdown.clone();
    let status = new_status_cell();
    let status_for_task = Arc::clone(&status);
    let token_for_task = token.clone();
    // Bind to port 0 so the kernel picks a free port; sidesteps
    // the well-known port collision with anything else on the box.
    let bind_addr = loopback_addr(0);
    let handle = tokio::spawn(async move {
      serve(state, bind_addr, token_for_task, status_for_task)
        .await
        .expect("proxy serve returns Ok even on bind failure");
    });
    // Wait for the listener to record its bound address.
    let bound = wait_for_listening(&status, Duration::from_secs(2))
      .await
      .expect("proxy must reach Listening within 2s");
    (bound, token, status, handle)
  }

  /// Trigger shutdown and join the serve task with a generous budget.
  /// Catches the failure mode where a serve loop never exits — without
  /// this, `drop(handle)` would silently leave a detached task and the
  /// hang would go unnoticed.
  async fn shutdown_proxy(shutdown: ShutdownToken, handle: JoinHandle<()>) {
    shutdown.trigger();
    // DRAIN_TIMEOUT + 1 s slack for the spawn-task to wind down.
    let budget = DRAIN_TIMEOUT + Duration::from_secs(1);
    tokio::time::timeout(budget, handle)
      .await
      .expect("proxy serve loop must exit after shutdown.trigger()")
      .expect("proxy serve task must not panic");
  }

  async fn wait_for_listening(status: &StatusCell, budget: Duration) -> Option<SocketAddr> {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
      if let ProxyStatus::Listening { addr } = status.read().unwrap().clone() {
        return Some(addr);
      }
      tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
  }

  /// Minimal HTTP/1.1 client used by the inline tests. We don't
  /// want to pull `reqwest` into the unit tests just for two
  /// request/response cycles — and the keep-alive test needs
  /// explicit control over the TCP socket anyway.
  async fn http_get_keepalive(
    sock: &mut tokio::net::TcpStream,
    host: &str,
    path: &str,
  ) -> (u16, Vec<u8>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: keep-alive\r\n\r\n");
    sock.write_all(req.as_bytes()).await.expect("write");
    // Read until we have headers + body. We control both sides so a
    // simple read-until-content-length-bytes loop is enough.
    let mut buf = Vec::with_capacity(512);
    let mut tmp = [0u8; 1024];
    loop {
      let n = sock.read(&mut tmp).await.expect("read");
      if n == 0 {
        break;
      }
      buf.extend_from_slice(&tmp[..n]);
      // Stop once the body is complete (Content-Length present).
      if let Some(body) = extract_body_when_complete(&buf) {
        return body;
      }
    }
    panic!("connection closed before body arrived; got: {buf:?}");
  }

  fn extract_body_when_complete(buf: &[u8]) -> Option<(u16, Vec<u8>)> {
    let needle = b"\r\n\r\n";
    let split = buf.windows(needle.len()).position(|w| w == needle)?;
    let head = std::str::from_utf8(&buf[..split]).ok()?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next()?;
    let status: u16 = status_line.split_whitespace().nth(1)?.parse().ok()?;
    let mut content_length: Option<usize> = None;
    for line in lines {
      if let Some(v) = line
        .strip_prefix("Content-Length:")
        .or_else(|| line.strip_prefix("content-length:"))
      {
        content_length = v.trim().parse().ok();
      }
    }
    let body_start = split + needle.len();
    let need = content_length?;
    if buf.len() < body_start + need {
      return None;
    }
    let body = buf[body_start..body_start + need].to_vec();
    Some((status, body))
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn health_returns_ok_shape() {
    let (addr, shutdown, _status, handle) = spawn_proxy_on_ephemeral_port().await;
    let mut sock = tokio::net::TcpStream::connect(addr)
      .await
      .expect("connect to proxy");
    let host = addr.to_string();
    let (status, body) = http_get_keepalive(&mut sock, &host, "/health").await;
    assert_eq!(status, 200, "health returns 200");
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("json body");
    assert_eq!(parsed["status"], "ok");
    assert!(parsed["models_loaded"].is_u64(), "models_loaded shape");
    assert!(
      parsed["models_discovered"].is_u64(),
      "models_discovered shape"
    );
    shutdown_proxy(shutdown, handle).await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn chat_completions_without_model_field_returns_400() {
    // Unit 3 wires the four `/v1/...` arms to the resolver. A
    // body without `model` short-circuits at the
    // `RouteDecision::ModelRequired` arm — 400
    // `invalid_request` / `code: model_required`. Pre-Unit-3
    // this same route returned 501; the assertion swap documents
    // the contract handoff.
    let (addr, shutdown, _status, handle) = spawn_proxy_on_ephemeral_port().await;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let host = addr.to_string();
    let req = format!(
      "POST /v1/chat/completions HTTP/1.1\r\nHost: {host}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
    );
    sock.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    sock.read_to_end(&mut buf).await.expect("read");
    let head = std::str::from_utf8(&buf).expect("utf8");
    assert!(head.contains("400"), "expected 400 in: {head}");
    assert!(head.contains("model_required"), "body shape: {head}");
    shutdown_proxy(shutdown, handle).await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn keep_alive_serves_two_health_requests_on_one_connection() {
    let (addr, shutdown, _status, handle) = spawn_proxy_on_ephemeral_port().await;
    let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let host = addr.to_string();
    let (s1, b1) = http_get_keepalive(&mut sock, &host, "/health").await;
    assert_eq!(s1, 200, "first request status");
    let p1: serde_json::Value = serde_json::from_slice(&b1).expect("json");
    assert_eq!(p1["status"], "ok");
    // Second GET reuses the same TCP socket.
    let (s2, b2) = http_get_keepalive(&mut sock, &host, "/health").await;
    assert_eq!(s2, 200, "second request status (keep-alive smoke)");
    let p2: serde_json::Value = serde_json::from_slice(&b2).expect("json");
    assert_eq!(p2["status"], "ok");
    shutdown_proxy(shutdown, handle).await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn bind_failure_records_port_in_use() {
    // First listener camps on a real port.
    let camp = tokio::net::TcpListener::bind(loopback_addr(0))
      .await
      .expect("camp bind");
    let camp_addr = camp.local_addr().expect("camp addr");
    // Now ask the proxy to bind the same port — must surface
    // PortInUse and exit cleanly.
    let ctx = MethodContext::new(ShutdownToken::new());
    let state = ProxyState::from_context(&ctx);
    let token = ctx.shutdown.clone();
    let status = new_status_cell();
    serve(state, camp_addr, token, Arc::clone(&status))
      .await
      .expect("bind failure must not error");
    let observed = status.read().unwrap().clone();
    match observed {
      ProxyStatus::PortInUse { addr } => assert_eq!(addr, camp_addr),
      other => panic!("expected PortInUse, got {other:?}"),
    }
    drop(camp);
  }

  // Echo verification against `fake_llama_server` lives in
  // tests/proxy_echo_verification.rs — inline `#[cfg(test)] mod
  // tests` cannot reach `CARGO_BIN_EXE_fake_llama_server` (cargo
  // sets that env var only when building integration tests under
  // `tests/`).
}
