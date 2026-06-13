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
  /// Resolved context window (`n_ctx`) the child loaded with — what
  /// `--fit` settled on. `None` when `/props` didn't expose it.
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
/// resolved context window. Best-effort: any error → empty `Actuals`.
pub async fn fetch(port: u16, timeout: Duration) -> Actuals {
  match fetch_props_body(port, timeout).await {
    Ok(body) => Actuals {
      resolved_ctx: parse_resolved_ctx(&body),
    },
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
/// JSON body for the resolved context window. llama-server has carried
/// `n_ctx` under `default_generation_settings` and (in some builds) at
/// the top level — try both so we survive a schema shuffle.
fn parse_resolved_ctx(response: &[u8]) -> Option<u32> {
  let split = response.windows(4).position(|w| w == b"\r\n\r\n")?;
  let body = &response[split + 4..];
  let v: serde_json::Value = serde_json::from_slice(body).ok()?;
  extract_n_ctx(&v)
}

fn extract_n_ctx(v: &serde_json::Value) -> Option<u32> {
  v.get("default_generation_settings")
    .and_then(|g| g.get("n_ctx"))
    .and_then(serde_json::Value::as_u64)
    .or_else(|| v.get("n_ctx").and_then(serde_json::Value::as_u64))
    .map(|n| n as u32)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_n_ctx_from_default_generation_settings() {
    let resp = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
      {\"default_generation_settings\":{\"n_ctx\":16384},\"total_slots\":1}";
    assert_eq!(parse_resolved_ctx(resp), Some(16384));
  }

  #[test]
  fn falls_back_to_top_level_n_ctx() {
    let resp = b"HTTP/1.1 200 OK\r\n\r\n{\"n_ctx\":8192}";
    assert_eq!(parse_resolved_ctx(resp), Some(8192));
  }

  #[test]
  fn missing_field_yields_none_not_a_crash() {
    let resp = b"HTTP/1.1 200 OK\r\n\r\n{\"total_slots\":1}";
    assert_eq!(parse_resolved_ctx(resp), None);
  }

  #[test]
  fn malformed_body_yields_none() {
    assert_eq!(parse_resolved_ctx(b"HTTP/1.1 200 OK\r\n\r\nnot json"), None);
    assert_eq!(parse_resolved_ctx(b"no headers no body"), None);
  }

  #[test]
  fn actuals_is_empty_when_unset() {
    assert!(Actuals::default().is_empty());
    assert!(!Actuals {
      resolved_ctx: Some(4096)
    }
    .is_empty());
  }
}
