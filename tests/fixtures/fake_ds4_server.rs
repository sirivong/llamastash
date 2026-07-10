//! Minimal stand-in for the real `ds4-server` binary, used by the ds4
//! integration tests. Hand-rolls just enough HTTP/1.1 over
//! `tokio::TcpListener` to answer the endpoints llamastash's supervisor,
//! readiness probe, orphan sweep, and proxy touch — and to reproduce the
//! ds4-specific divergences that shaped the backend:
//!
//! - **load-before-listen**: the real `ds4-server` fully loads the model
//!   (`ds4_engine_open`) *before* binding its listener. `--load-delay-ms <n>`
//!   models that window — the socket stays unbound for `n` ms, so the
//!   readiness probe observes the reserved port refusing connections until
//!   the "weights" are "loaded".
//! - **fixed alias**: `GET /v1/models` reports `deepseek-v4-flash` (or the
//!   `--alias <id>` override), never the file path — the reason adoption +
//!   readiness match the alias set, not the `-m` value. `--alias` lets a test
//!   stand up a *foreign* server (wrong id) on the reserved port to prove the
//!   probe's alias-body check.
//! - **no `/health`, no web UI**: everything outside the served routes 404s.
//!
//! Endpoints: `GET /v1/models`, `POST /v1/chat/completions` (SSE +
//! non-stream), `/v1/completions`, `/v1/messages` (Anthropic). Args mirror
//! the real invocation: `-m/--model`, `--host`, `--port`, `--ctx/-c`, plus
//! the native-knob flags (`--power`/`--tokens`/`--threads`/`--kv-disk-*`/
//! `--ssd-streaming`) which are accepted and ignored. Body-marker failure
//! injection mirrors `fake_llama_server` (a message containing `fail` → 500).

use std::env;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::main(flavor = "current_thread")]
async fn main() {
  let cfg = parse_args();
  // Model "load" happens before the listener binds — the reserved port is
  // unbound for this whole window (the real load-before-listen behaviour).
  if cfg.load_delay_ms > 0 {
    tokio::time::sleep(Duration::from_millis(cfg.load_delay_ms)).await;
  }
  let listener = TcpListener::bind((cfg.host.as_str(), cfg.port))
    .await
    .expect("fake_ds4_server: bind");
  loop {
    let Ok((mut sock, _)) = listener.accept().await else {
      break;
    };
    let alias = cfg.alias.clone();
    tokio::spawn(async move {
      let raw = read_request(&mut sock).await;
      route_and_reply(&mut sock, &raw, &alias).await;
    });
  }
}

struct Config {
  host: String,
  port: u16,
  alias: String,
  load_delay_ms: u64,
}

fn parse_args() -> Config {
  let args: Vec<String> = env::args().collect();
  let mut host = "127.0.0.1".to_string();
  let mut port = 8000u16;
  let mut alias = "deepseek-v4-flash".to_string();
  let mut load_delay_ms = 0u64;
  let mut i = 1;
  while i < args.len() {
    match args[i].as_str() {
      "--host" => {
        if let Some(v) = args.get(i + 1) {
          host = v.clone();
          i += 1;
        }
      }
      "--port" => {
        if let Some(v) = args.get(i + 1).and_then(|s| s.parse().ok()) {
          port = v;
          i += 1;
        }
      }
      "--alias" => {
        if let Some(v) = args.get(i + 1) {
          alias = v.clone();
          i += 1;
        }
      }
      "--load-delay-ms" => {
        if let Some(v) = args.get(i + 1).and_then(|s| s.parse().ok()) {
          load_delay_ms = v;
          i += 1;
        }
      }
      // Every other flag (-m, --ctx, --power, --ssd-streaming, …) is
      // accepted and ignored — the fixture doesn't run a real model.
      _ => {}
    }
    i += 1;
  }
  Config {
    host,
    port,
    alias,
    load_delay_ms,
  }
}

async fn route_and_reply(sock: &mut tokio::net::TcpStream, raw: &str, alias: &str) {
  let (method, path) = request_line(raw);
  match (method.as_str(), path.as_str()) {
    ("GET", "/v1/models") => {
      let body = format!(
        "{{\"object\":\"list\",\"data\":[{{\"id\":\"{alias}\",\"object\":\"model\",\"owned_by\":\"ds4.c\"}}]}}"
      );
      write_json(sock, 200, &body).await;
    }
    ("POST", "/v1/chat/completions") => {
      if raw.contains("fail") {
        write_json(sock, 500, "{\"error\":{\"message\":\"injected failure\"}}").await;
        return;
      }
      if raw.contains("\"stream\":true") || raw.contains("\"stream\": true") {
        write_sse(sock, alias).await;
      } else {
        let body = format!(
          "{{\"id\":\"cmpl-fake\",\"object\":\"chat.completion\",\"model\":\"{alias}\",\
           \"choices\":[{{\"index\":0,\"message\":{{\"role\":\"assistant\",\"content\":\"ok\"}},\
           \"finish_reason\":\"stop\"}}]}}"
        );
        write_json(sock, 200, &body).await;
      }
    }
    ("POST", "/v1/completions") => {
      let body = format!(
        "{{\"id\":\"cmpl-fake\",\"object\":\"text_completion\",\"model\":\"{alias}\",\
         \"choices\":[{{\"index\":0,\"text\":\"ok\",\"finish_reason\":\"stop\"}}]}}"
      );
      write_json(sock, 200, &body).await;
    }
    ("POST", "/v1/messages") => {
      // Anthropic-shaped (byte-forwarded by the proxy; ds4 converts natively).
      let body = format!(
        "{{\"id\":\"msg_fake\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"{alias}\",\
         \"content\":[{{\"type\":\"text\",\"text\":\"ok\"}}],\"stop_reason\":\"end_turn\"}}"
      );
      write_json(sock, 200, &body).await;
    }
    // No `/health`, no `/`, no `/ui` — everything else 404s, like the real
    // ds4-server.
    _ => write_json(sock, 404, "{\"error\":{\"message\":\"not found\"}}").await,
  }
}

async fn write_json(sock: &mut tokio::net::TcpStream, status: u16, body: &str) {
  let reason = if status == 200 { "OK" } else { "ERR" };
  let resp = format!(
    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
    body.len()
  );
  let _ = sock.write_all(resp.as_bytes()).await;
}

/// A minimal OpenAI-style SSE stream: one content chunk then `[DONE]`.
async fn write_sse(sock: &mut tokio::net::TcpStream, alias: &str) {
  let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
  let _ = sock.write_all(headers.as_bytes()).await;
  let chunk = format!(
    "data: {{\"id\":\"cmpl-fake\",\"object\":\"chat.completion.chunk\",\"model\":\"{alias}\",\
     \"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"ok\"}}}}]}}\n\n"
  );
  let _ = sock.write_all(chunk.as_bytes()).await;
  let _ = sock.write_all(b"data: [DONE]\n\n").await;
}

fn request_line(raw: &str) -> (String, String) {
  let first = raw.lines().next().unwrap_or_default();
  let mut parts = first.split_whitespace();
  let method = parts.next().unwrap_or_default().to_string();
  let path = parts.next().unwrap_or_default().to_string();
  (method, path)
}

async fn read_request(sock: &mut tokio::net::TcpStream) -> String {
  // Read until the header/body boundary, then drain any declared body so the
  // chat routes can inspect the request (stream flag / failure marker).
  let mut buf = vec![0u8; 8192];
  let mut acc = Vec::new();
  loop {
    let n = match sock.read(&mut buf).await {
      Ok(0) | Err(_) => break,
      Ok(n) => n,
    };
    acc.extend_from_slice(&buf[..n]);
    let text = String::from_utf8_lossy(&acc);
    if let Some(hdr_end) = text.find("\r\n\r\n") {
      let content_len = text
        .lines()
        .find_map(|l| {
          let l = l.to_ascii_lowercase();
          l.strip_prefix("content-length:")
            .and_then(|v| v.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
      let body_have = acc.len() - (hdr_end + 4);
      if body_have >= content_len {
        break;
      }
    }
    if acc.len() > 64 * 1024 {
      break;
    }
  }
  String::from_utf8_lossy(&acc).into_owned()
}
