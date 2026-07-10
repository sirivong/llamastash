//! Fixture-backed integration coverage for the ds4 (DwarfStar) backend,
//! driven through the production `run_foreground` daemon against the
//! `fake_ds4_server` fixture (which mirrors the real load-before-listen +
//! fixed-alias + no-`/health` contract).
//!
//! Covers the ds4 lifecycle a unit test can't reach: a ds4-compatible GGUF
//! auto-routes to ds4, spawns `ds4-server`, reaches Ready via the
//! `/v1/models` alias body (not the file path), reports the deepseek4
//! KV-blind advisory, and — with ds4 unavailable — falls back to llama.cpp
//! with no error.
//!
//! Built only under `--features test-fixtures`.

#![cfg(feature = "test-fixtures")]

use std::path::{Path, PathBuf};
use std::time::Duration;

use llamastash::config::loader::{Ds4Config, PortRange};
use llamastash::daemon::{run_foreground, DaemonOptions};
use llamastash::gguf::test_fixtures::FixtureBuilder;
use llamastash::ipc::Client;
use serde_json::{json, Value};

fn fake_llama_binary() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake_llama_server"))
}

fn fake_ds4_binary() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake_ds4_server"))
}

fn unique_temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-ds4", label)
}

/// A synthetic **ds4-compatible** GGUF: `deepseek4` arch + the per-tensor
/// quant contract (routed experts IQ2_XXS, everything else Q8_0 / F16 / I32).
fn ds4_gguf_bytes() -> Vec<u8> {
  const IQ2_XXS: u32 = 16;
  const Q8_0: u32 = 8;
  const F16: u32 = 1;
  const I32: u32 = 26;
  FixtureBuilder::new()
    .with_arch("deepseek4")
    .with_context_length(8192)
    .with_tensor("blk.0.ffn_gate_exps.weight", &[512, 512], IQ2_XXS)
    .with_tensor("blk.0.ffn_down_exps.weight", &[512, 512], IQ2_XXS)
    .with_tensor("blk.0.ffn_gate_tid2eid.weight", &[512], I32)
    .with_tensor("blk.0.attn_q.weight", &[512, 512], Q8_0)
    .with_tensor("token_embd.weight", &[512, 512], F16)
    .build()
}

async fn wait_for_socket(path: &Path) {
  let deadline = std::time::Instant::now() + Duration::from_secs(30);
  loop {
    if std::time::Instant::now() > deadline {
      panic!("daemon socket never appeared: {}", path.display());
    }
    if Client::connect(path).await.is_ok() {
      return;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
}

fn allocate_port_range() -> PortRange {
  let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
  let port = listener.local_addr().unwrap().port();
  drop(listener);
  PortRange {
    start: port,
    end: port,
  }
}

/// Poll `status` until the single model row reaches `ready` / `error` or the
/// budget runs out. Returns the terminal row.
async fn wait_ready(client: &mut Client) -> Value {
  // The `status` model row nests its lifecycle under `state`:
  // `{ "state": { "state": "ready", "port": N, ... } }`.
  let deadline = std::time::Instant::now() + Duration::from_secs(20);
  loop {
    let status = client.call("status", None).await.expect("status");
    if let Some(row) = status
      .get("models")
      .and_then(|m| m.as_array())
      .and_then(|a| a.first())
    {
      let state = row
        .get("state")
        .and_then(|s| s.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("");
      if state == "ready" || state == "error" {
        return row.clone();
      }
    }
    if std::time::Instant::now() > deadline {
      let status = client.call("status", None).await.expect("status");
      panic!("model never settled; status={status}");
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
  }
}

/// Read the nested `state.state` label from a `status` model row.
fn row_state(row: &Value) -> &str {
  row
    .get("state")
    .and_then(|s| s.get("state"))
    .and_then(Value::as_str)
    .unwrap_or("")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ds4_compatible_model_auto_routes_to_ds4_reaches_ready_and_stops() {
  let state = unique_temp("happy");
  let model_dir = unique_temp("happy-models");
  let model_path = model_dir.join("deepseek-v4-flash.gguf");
  std::fs::write(&model_path, ds4_gguf_bytes()).unwrap();

  let opts = DaemonOptions {
    binary: Some(fake_llama_binary()),
    port_range: allocate_port_range(),
    ds4: Ds4Config {
      enabled: Some(true),
      binary: Some(fake_ds4_binary()),
    },
    ..DaemonOptions::rooted_at(state.clone())
  };
  let socket = opts.state_dir.clone();
  let daemon = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;
  let mut client = Client::connect(&socket).await.expect("connect");

  // A ds4-compatible model, no `--backend`: must auto-route to ds4 and carry
  // the deepseek4 KV-blind advisory in the response.
  let start = client
    .call(
      "start_model",
      Some(json!({ "model_path": model_path.to_string_lossy() })),
    )
    .await
    .expect("start_model");
  let warnings: Vec<String> = start
    .get("warnings")
    .and_then(Value::as_array)
    .map(|a| {
      a.iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
    })
    .unwrap_or_default();
  assert!(
    warnings.iter().any(|w| w.contains("deepseek4")),
    "expected the KV-blind deepseek4 advisory, got {warnings:?}"
  );

  // Reaches Ready via the alias body probe (the fixture delays its bind, so
  // this exercises the load-before-listen window).
  let row = wait_ready(&mut client).await;
  assert_eq!(
    row_state(&row),
    "ready",
    "ds4 model must reach Ready (row: {row})"
  );

  // The running port answers `/v1/models` with the ds4 alias — proof the
  // ds4-server fixture (not the llama fixture) is behind the port.
  let port = start.get("port").and_then(Value::as_u64).expect("port") as u16;
  let body = reqwest::get(format!("http://127.0.0.1:{port}/v1/models"))
    .await
    .expect("GET /v1/models")
    .text()
    .await
    .unwrap();
  assert!(
    body.contains("deepseek-v4-flash"),
    "ds4 /v1/models must advertise the alias, got {body}"
  );

  // Stop cleanly (SIGTERM path — the fixture has no `/health`).
  let launch_id = start.get("launch_id").and_then(Value::as_str).unwrap();
  client
    .call("stop_model", Some(json!({ "launch_id": launch_id })))
    .await
    .expect("stop_model");

  client.call("shutdown", None).await.ok();
  let _ = tokio::time::timeout(Duration::from_secs(5), daemon).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ds4_compatible_model_falls_back_to_llamacpp_when_ds4_unavailable() {
  let state = unique_temp("fallback");
  let model_dir = unique_temp("fallback-models");
  let model_path = model_dir.join("deepseek-v4-flash.gguf");
  std::fs::write(&model_path, ds4_gguf_bytes()).unwrap();

  // ds4 disabled → the same compatible model must run on llama.cpp (the fake
  // llama server), never refuse.
  let opts = DaemonOptions {
    binary: Some(fake_llama_binary()),
    port_range: allocate_port_range(),
    ds4: Ds4Config {
      enabled: Some(false),
      binary: Some(fake_ds4_binary()),
    },
    ..DaemonOptions::rooted_at(state.clone())
  };
  let socket = opts.state_dir.clone();
  let daemon = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;
  let mut client = Client::connect(&socket).await.expect("connect");

  let start = client
    .call(
      "start_model",
      Some(json!({ "model_path": model_path.to_string_lossy() })),
    )
    .await
    .expect("start_model must succeed on llama.cpp fallback");
  let row = wait_ready(&mut client).await;
  assert_eq!(
    row_state(&row),
    "ready",
    "fallback to llama.cpp must reach Ready (row: {row})"
  );

  // The llama fixture answers `/health` (ds4 has none) — a quick way to prove
  // the fallback binary, not ds4-server, is behind the port.
  let port = start.get("port").and_then(Value::as_u64).expect("port") as u16;
  let health = reqwest::get(format!("http://127.0.0.1:{port}/health"))
    .await
    .expect("GET /health")
    .status();
  assert!(
    health.is_success(),
    "llama.cpp fallback must answer /health; got {health}"
  );

  let launch_id = start.get("launch_id").and_then(Value::as_str).unwrap();
  client
    .call("stop_model", Some(json!({ "launch_id": launch_id })))
    .await
    .ok();
  client.call("shutdown", None).await.ok();
  let _ = tokio::time::timeout(Duration::from_secs(5), daemon).await;
}
