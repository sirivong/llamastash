//! Post-launch actuals: what `--fit` actually chose, read once from the
//! child's `/props` after it reaches Ready (R6).
//!
//! Since placement is delegated to `--fit`, llamastash does not know the
//! resolved context window (or layer split) until the child is up. We
//! fetch it once on the Loading→Ready transition and surface it on
//! `status` (and thus the TUI Running view + `show`). Best-effort: a
//! build whose `/props` omits the field, or any transport error, yields
//! `None` and the surfaces render "unavailable" rather than a wrong
//! number.
//!
//! Transport is a hand-rolled `GET /props` over raw TCP with
//! `Connection: close` (so we can read to EOF without a keep-alive
//! dance) — the same no-dep stance as [`crate::daemon::probe`]. We never
//! pull in `reqwest`/`hyper` just for this.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// What the child reports it actually loaded with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Actuals {
  /// Resolved **total** context window the child loaded with — what
  /// `--fit` (or a pin) settled on, matching the `--ctx` / `-c` knob.
  /// `None` when `/props` didn't expose it. This is the one placement
  /// value llama-server's HTTP API reports; the rest (layers, threads,
  /// batch) live only in load-time logs, so the TUI shows those as
  /// `auto`.
  ///
  /// `/props` reports `default_generation_settings.n_ctx` as the
  /// **per-slot** window (`total / total_slots`), so this is rebuilt as
  /// `n_ctx * total_slots` to match the `-c` value the user sees and
  /// pins. Verified against a `-c 8192 --parallel 4` launch: `/props`
  /// reports `n_ctx=2048, total_slots=4`, which is `8192` total.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub resolved_ctx: Option<u32>,
}

impl Actuals {
  /// True when nothing was captured — surfaces render "unavailable".
  pub fn is_empty(&self) -> bool {
    self.resolved_ctx.is_none()
  }
}

/// Fetch `/props` from the child on `127.0.0.1:<port>` and extract the
/// values `--fit` resolved. Best-effort: any error → empty `Actuals`.
pub async fn fetch(port: u16, timeout: Duration) -> Actuals {
  match fetch_props_body(port, timeout).await {
    Ok(body) => parse_actuals(&body),
    Err(_) => Actuals::default(),
  }
}

async fn fetch_props_body(port: u16, timeout: Duration) -> std::io::Result<Vec<u8>> {
  let request = format!(
    "GET /props HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nAccept: application/json\r\n\r\n"
  );
  let fut = async {
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await?;
    sock.write_all(request.as_bytes()).await?;
    let mut buf = Vec::with_capacity(4096);
    // `Connection: close` makes the server hang up after the body, so a
    // read-to-EOF terminates. Cap the buffer so a misbehaving peer can't
    // stream unbounded data into the daemon.
    let mut chunk = [0u8; 4096];
    loop {
      let n = sock.read(&mut chunk).await?;
      if n == 0 {
        break;
      }
      buf.extend_from_slice(&chunk[..n]);
      if buf.len() > 256 * 1024 {
        break;
      }
    }
    Ok::<_, std::io::Error>(buf)
  };
  tokio::time::timeout(timeout, fut)
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "props fetch timeout"))?
}

/// Split the HTTP response at the header/body boundary and parse the
/// JSON body for the resolved context window. llama-server carries the
/// per-slot `n_ctx` under `default_generation_settings` (and at the top
/// level in some builds) and the slot count at the top-level
/// `total_slots`; the **total** window is `n_ctx * total_slots`.
fn parse_actuals(response: &[u8]) -> Actuals {
  let Some(split) = response.windows(4).position(|w| w == b"\r\n\r\n") else {
    return Actuals::default();
  };
  let body = &response[split + 4..];
  let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
    return Actuals::default();
  };
  Actuals {
    resolved_ctx: extract_total_ctx(&v),
  }
}

/// Per-slot context window from `/props`.
fn extract_n_ctx(v: &serde_json::Value) -> Option<u32> {
  v.get("default_generation_settings")
    .and_then(|g| g.get("n_ctx"))
    .and_then(serde_json::Value::as_u64)
    .or_else(|| v.get("n_ctx").and_then(serde_json::Value::as_u64))
    .map(|n| n as u32)
}

/// Total context window = per-slot `n_ctx` * `total_slots`. `total_slots`
/// defaults to 1 when absent (older builds / `np=1`), so the value
/// degrades to the per-slot reading rather than vanishing.
fn extract_total_ctx(v: &serde_json::Value) -> Option<u32> {
  let per_slot = extract_n_ctx(v)?;
  let slots = v
    .get("total_slots")
    .and_then(serde_json::Value::as_u64)
    .filter(|&n| n > 0)
    .unwrap_or(1) as u32;
  Some(per_slot.saturating_mul(slots))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_n_ctx_from_default_generation_settings() {
    let resp = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
      {\"default_generation_settings\":{\"n_ctx\":16384},\"total_slots\":1}";
    assert_eq!(parse_actuals(resp).resolved_ctx, Some(16384));
  }

  #[test]
  fn falls_back_to_top_level_n_ctx() {
    let resp = b"HTTP/1.1 200 OK\r\n\r\n{\"n_ctx\":8192}";
    assert_eq!(parse_actuals(resp).resolved_ctx, Some(8192));
  }

  #[test]
  fn total_ctx_is_per_slot_times_slots() {
    // `/props` reports the per-slot window; the total is x total_slots.
    // Verified live against `-c 8192 --parallel 4`: n_ctx=2048, slots=4.
    let resp = b"HTTP/1.1 200 OK\r\n\r\n\
      {\"default_generation_settings\":{\"n_ctx\":2048},\"total_slots\":4}";
    assert_eq!(parse_actuals(resp).resolved_ctx, Some(8192));
    // total_slots absent → degrade to the per-slot reading (np=1).
    let resp1 = b"HTTP/1.1 200 OK\r\n\r\n{\"default_generation_settings\":{\"n_ctx\":4096}}";
    assert_eq!(parse_actuals(resp1).resolved_ctx, Some(4096));
  }

  #[test]
  fn missing_field_yields_none_not_a_crash() {
    let resp = b"HTTP/1.1 200 OK\r\n\r\n{\"model_path\":\"x\"}";
    assert_eq!(parse_actuals(resp).resolved_ctx, None);
  }

  #[test]
  fn malformed_body_yields_none() {
    assert!(parse_actuals(b"HTTP/1.1 200 OK\r\n\r\nnot json").is_empty());
    assert!(parse_actuals(b"no headers no body").is_empty());
  }

  #[test]
  fn actuals_is_empty_when_unset() {
    assert!(Actuals::default().is_empty());
    assert!(!Actuals {
      resolved_ctx: Some(4096),
    }
    .is_empty());
  }
}
