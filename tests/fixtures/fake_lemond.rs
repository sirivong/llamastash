//! Minimal stand-in for the `lemond` (Lemonade umbrella) binary, used by
//! the managed-multiplexer integration test. Hand-rolls just enough
//! HTTP/1.1 over `tokio::TcpListener` to answer the endpoints
//! llamastash's umbrella orchestration + client touch:
//!
//! - `GET  /live`            → 200 (umbrella liveness — supervisor probe)
//! - `GET  /api/v1/health`   → 200 status + loaded model
//! - `GET  /api/v1/models`   → 200 OpenAI-shaped model list
//! - `POST /api/v1/load`     → 200 success
//! - `POST /api/v1/unload`   → 200 success
//! - `POST /api/v1/chat/completions` → 200 OpenAI-shaped chat (proxy route test)
//!
//! Args mirror the real `lemond` invocation enough for the supervisor's
//! argv to be portable: `--host <ADDR>` / `--port <N>`, plus an optional
//! positional cache-dir (accepted and ignored — the supervisor no longer
//! passes one, but the real `lemond` would). CI has no real `lemond`, so
//! the managed-multiplexer test runs against this binary.

use std::env;
use std::sync::Mutex;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The currently-resident model name, so `/api/v1/{load,unload}` mutate
/// state and `/api/v1/health` reports it — lets the eviction test assert
/// that an idle unload actually cleared the loaded model.
static LOADED: Mutex<Option<String>> = Mutex::new(None);

#[tokio::main(flavor = "current_thread")]
async fn main() {
  let (host, port) = parse_args();
  let listener = TcpListener::bind((host.as_str(), port))
    .await
    .expect("fake_lemond: bind");
  loop {
    let Ok((mut sock, _)) = listener.accept().await else {
      break;
    };
    tokio::spawn(async move {
      let raw = read_request(&mut sock).await;
      let (status, body) = route(&raw).await;
      let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
      );
      let _ = sock.write_all(resp.as_bytes()).await;
    });
  }
}

/// Route on the full raw request. `/api/v1/{load,unload}` mutate the
/// process-global loaded-model state and `/api/v1/health` reports it, so
/// the eviction test can assert an idle unload cleared the model.
///
/// Failure / latency injection mirrors `fake_llama_server`'s body-marker
/// pattern, keyed on the requested model name so concurrent tests never
/// race an env var: a name containing `fail` rejects the load with 500;
/// `slow` sleeps ~1.5 s before succeeding — far past any caller that
/// wrongly blocks its own reply path on the load.
async fn route(raw: &str) -> (String, String) {
  let request_line = raw.lines().next().unwrap_or_default();
  let mut parts = request_line.split_whitespace();
  let method = parts.next().unwrap_or("");
  let path = parts.next().unwrap_or("");
  match (method, path) {
    ("GET", "/live") => ok(r#"{"status":"ok"}"#),
    ("GET", "/api/v1/health") => {
      let model = match LOADED.lock().unwrap().clone() {
        Some(m) => format!("\"{m}\""),
        None => "null".to_string(),
      };
      ok(&format!(r#"{{"status":"ok","model_loaded":{model}}}"#))
    }
    ("GET", "/api/v1/models") => ok(
      r#"{"object":"list","data":[{"id":"Qwen2.5-0.5B-Instruct","recipe":"llamacpp"},{"id":"Llama-3.1-8B","recipe":"llamacpp"}]}"#,
    ),
    ("POST", "/api/v1/load") => {
      let name = body_model_name(raw).unwrap_or_else(|| "unknown".to_string());
      if name.contains("fail") {
        return (
          "500 Internal Server Error".to_string(),
          r#"{"detail":"injected load failure"}"#.to_string(),
        );
      }
      if name.contains("slow") {
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
      }
      *LOADED.lock().unwrap() = Some(name);
      ok(r#"{"status":"success","message":"loaded"}"#)
    }
    ("POST", "/api/v1/unload") => {
      *LOADED.lock().unwrap() = None;
      ok(r#"{"status":"success"}"#)
    }
    // OpenAI-shaped chat completion. The proxy rewrites an inbound
    // `/v1/chat/completions` to `/api/v1/...` for Lemonade upstreams; the
    // recognizable body lets the route test assert the request landed here.
    ("POST", "/api/v1/chat/completions") => ok(
      r#"{"id":"lemonade-chat-1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"hi from lemond"}}]}"#,
    ),
    _ => (
      "404 Not Found".to_string(),
      r#"{"detail":"not found"}"#.to_string(),
    ),
  }
}

/// Read a full HTTP/1.1 request: headers through `\r\n\r\n`, then
/// `Content-Length` body bytes. A single `read()` is not enough — the
/// client may deliver headers and body in separate packets (seen on the
/// coverage-instrumented CI lane), and routing on headers alone made the
/// fixture answer `/api/v1/load` before the body's model name arrived,
/// then close the socket mid-upload — resetting the client's request.
async fn read_request(sock: &mut tokio::net::TcpStream) -> String {
  let mut buf = Vec::with_capacity(4096);
  let mut chunk = [0u8; 4096];
  let header_end = loop {
    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
      break pos + 4;
    }
    match sock.read(&mut chunk).await {
      Ok(0) | Err(_) => return String::from_utf8_lossy(&buf).into_owned(),
      Ok(n) => buf.extend_from_slice(&chunk[..n]),
    }
  };
  let content_length = String::from_utf8_lossy(&buf[..header_end])
    .lines()
    .find_map(|l| {
      let lower = l.to_ascii_lowercase();
      let value = lower.strip_prefix("content-length:")?;
      value.trim().parse::<usize>().ok()
    })
    .unwrap_or(0);
  while buf.len() < header_end + content_length {
    match sock.read(&mut chunk).await {
      Ok(0) | Err(_) => break,
      Ok(n) => buf.extend_from_slice(&chunk[..n]),
    }
  }
  String::from_utf8_lossy(&buf).into_owned()
}

/// Build a `(status, body)` pair for a 200 response.
fn ok(body: &str) -> (String, String) {
  ("200 OK".to_string(), body.to_string())
}

/// Crude extract of `"model_name":"<value>"` from a request body — enough
/// for the fixture to track which model `load` made resident.
fn body_model_name(raw: &str) -> Option<String> {
  let body = raw.split("\r\n\r\n").nth(1)?;
  let key = "\"model_name\"";
  let after = &body[body.find(key)? + key.len()..];
  let rest = after[after.find(':')? + 1..].trim_start();
  let rest = rest.strip_prefix('"')?;
  Some(rest[..rest.find('"')?].to_string())
}

/// Parse `[<dir>] --host <addr> --port <n>`. The positional working
/// directory is accepted and ignored (the real `lemond` stores config
/// there); host/port default to Lemonade's own defaults.
fn parse_args() -> (String, u16) {
  let mut host = "127.0.0.1".to_string();
  let mut port: u16 = 13305;
  let args: Vec<String> = env::args().skip(1).collect();
  let mut i = 0;
  while i < args.len() {
    match args[i].as_str() {
      "--host" => {
        if let Some(v) = args.get(i + 1) {
          host = v.clone();
          i += 1;
        }
      }
      "--port" => {
        if let Some(v) = args.get(i + 1) {
          port = v.parse().unwrap_or(13305);
          i += 1;
        }
      }
      _ => { /* positional working-dir — ignored */ }
    }
    i += 1;
  }
  (host, port)
}
