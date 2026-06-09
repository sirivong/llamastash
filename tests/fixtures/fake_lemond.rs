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
//! argv to be portable: a positional working-dir (ignored) plus
//! `--host <ADDR>` / `--port <N>`. CI has no real `lemond`, so the
//! managed-multiplexer test runs against this binary.

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
      let mut buf = vec![0u8; 4096];
      let n = sock.read(&mut buf).await.unwrap_or(0);
      let raw = String::from_utf8_lossy(&buf[..n]).into_owned();
      let (status, body) = route(&raw);
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
fn route(raw: &str) -> (String, String) {
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
    ("GET", "/api/v1/models") => {
      ok(r#"{"object":"list","data":[{"id":"Qwen2.5-0.5B-Instruct"},{"id":"Llama-3.1-8B"}]}"#)
    }
    ("POST", "/api/v1/load") => {
      let name = body_model_name(raw).unwrap_or_else(|| "unknown".to_string());
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
