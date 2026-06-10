//! Typed async client for the `lemond` HTTP API.
//!
//! Only the endpoints llamastash's managed-multiplexer orchestration needs
//! (R10/R11), all on the loopback `lemond` the supervisor runs:
//!
//! | Method | Path | Wrapper |
//! |---|---|---|
//! | `GET`  | `/live`            | [`LemonadeClient::live`] (umbrella liveness) |
//! | `GET`  | `/api/v1/health`   | [`LemonadeClient::health`] (loaded models) |
//! | `GET`  | `/api/v1/models`   | [`LemonadeClient::list_models`] (OpenAI list) |
//! | `POST` | `/api/v1/load`     | [`LemonadeClient::load`] (preload a model) |
//! | `POST` | `/api/v1/unload`   | [`LemonadeClient::unload`] |
//!
//! `lemond`'s chat-completions endpoint autoloads, so inference itself does
//! **not** need [`load`](LemonadeClient::load) — it rides the existing
//! OpenAI-compat proxy forward unchanged. `load` is an optional preload.
//!
//! HTTP goes through `reqwest` (already a workspace dep). The client is
//! built with `.no_proxy()` so a `HTTP_PROXY` in the environment can't
//! redirect a loopback call off-box.

use std::time::Duration;

use serde::Deserialize;

/// Default request timeout for a single `lemond` API call. Loopback calls
/// are fast; a model *load* can take longer, but `lemond` returns the
/// load response only once the model is resident, so the budget must cover
/// a real load. 120 s mirrors the probe's base budget.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// What can go wrong talking to `lemond`. The variants are deliberately
/// distinct so the orchestrator can tell "umbrella isn't up" (transport)
/// from "umbrella rejected the request" (HTTP / API).
#[derive(Debug, thiserror::Error)]
pub enum LemonadeError {
  /// Could not reach `lemond` at all (connection refused, timeout, DNS).
  /// For the orchestrator this means the umbrella is not (yet) serving.
  #[error("lemond transport error: {0}")]
  Transport(String),
  /// `lemond` answered with a non-success HTTP status.
  #[error("lemond returned HTTP {status}")]
  Http { status: u16 },
  /// `lemond` answered 2xx but the body did not parse as expected.
  #[error("lemond response decode error: {0}")]
  Decode(String),
  /// `lemond` answered 2xx but reported a logical failure in the body
  /// (e.g. `{"status":"error","message":...}` from load/unload).
  #[error("lemond API error: {0}")]
  Api(String),
}

/// One model row from `GET /api/v1/models` (OpenAI list shape). Beyond
/// the OpenAI `id`, `lemond` decorates rows with capability `labels`
/// (`transcription`, `embedding`, `vision`, …) and an approximate `size`
/// in GB — enough for discovery to project a mode hint and size for a
/// registry model that has no local file to read.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
  pub id: String,
  #[serde(default)]
  pub labels: Vec<String>,
  #[serde(default)]
  pub size: Option<f64>,
  /// Serving engine (`llamacpp`, `whispercpp`, `flm`, …). Names the
  /// `*_args` load-option field that engine reads.
  #[serde(default)]
  pub recipe: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModelList {
  #[serde(default)]
  data: Vec<ModelEntry>,
}

/// Subset of `GET /api/v1/health` llamastash reads.
#[derive(Debug, Clone, Deserialize)]
pub struct HealthResponse {
  /// `lemond`'s overall status string (e.g. `"ok"`).
  #[serde(default)]
  pub status: String,
  /// The currently-loaded model, if any.
  #[serde(default)]
  pub model_loaded: Option<String>,
}

/// `{"status": "...", "message": "..."}` from load / unload / pull.
#[derive(Debug, Deserialize)]
struct StatusResponse {
  #[serde(default)]
  status: String,
  #[serde(default)]
  message: String,
}

/// Launch options forwarded on `POST /api/v1/load` beyond the model
/// name — the subset of llamastash launch params lemond honors.
#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
  /// Context-size override; lemond's `ctx_size` field.
  pub ctx_size: Option<u32>,
  /// Recipe-scoped passthrough args: the JSON field name
  /// (`llamacpp_args` / `whispercpp_args` / `flm_args` — named after the
  /// model's recipe) and the space-joined argument string lemond passes
  /// to that engine. lemond strips flags it owns (`-m`, `--port`,
  /// `--ctx-size`, `-ngl`, …) on its side.
  pub recipe_args: Option<(String, String)>,
}

/// Build the `/api/v1/load` request body. Pure so the wire shape is
/// unit-testable without a live `lemond`.
fn load_body(model_name: &str, opts: &LoadOptions) -> serde_json::Value {
  let mut body = serde_json::json!({ "model_name": model_name });
  if let Some(ctx) = opts.ctx_size {
    body["ctx_size"] = ctx.into();
  }
  if let Some((field, value)) = &opts.recipe_args {
    body[field.as_str()] = serde_json::Value::String(value.clone());
  }
  body
}

/// A loopback client bound to one `lemond` instance.
#[derive(Debug, Clone)]
pub struct LemonadeClient {
  base: String,
  http: reqwest::Client,
}

impl LemonadeClient {
  /// Build a client for the `lemond` listening on `127.0.0.1:<port>`.
  pub fn new(port: u16) -> Result<Self, LemonadeError> {
    let http = reqwest::Client::builder()
      .no_proxy()
      .timeout(DEFAULT_TIMEOUT)
      .build()
      .map_err(|e| LemonadeError::Transport(e.to_string()))?;
    Ok(Self::with_client(port, http))
  }

  /// Build a client around a caller-supplied `reqwest::Client` (tests, or
  /// sharing one pooled client across calls).
  pub fn with_client(port: u16, http: reqwest::Client) -> Self {
    Self {
      base: format!("http://127.0.0.1:{port}"),
      http,
    }
  }

  /// `GET /live` — is the umbrella process up and serving? `Ok(())` on a
  /// success status; [`LemonadeError::Transport`] when `lemond` is unreachable.
  pub async fn live(&self) -> Result<(), LemonadeError> {
    let resp = self
      .http
      .get(format!("{}/live", self.base))
      .send()
      .await
      .map_err(transport)?;
    ensure_success(&resp)?;
    Ok(())
  }

  /// `GET /api/v1/health` — status + the currently-loaded model.
  pub async fn health(&self) -> Result<HealthResponse, LemonadeError> {
    let resp = self
      .http
      .get(format!("{}/api/v1/health", self.base))
      .send()
      .await
      .map_err(transport)?;
    ensure_success(&resp)?;
    decode(resp).await
  }

  /// `GET /api/v1/models` — the registry model names (OpenAI list).
  pub async fn list_models(&self) -> Result<Vec<String>, LemonadeError> {
    Ok(
      self
        .list_model_entries()
        .await?
        .into_iter()
        .map(|m| m.id)
        .collect(),
    )
  }

  /// `GET /api/v1/models` — the full rows (name + labels + size).
  /// Discovery uses the labels to derive a mode hint per model.
  pub async fn list_model_entries(&self) -> Result<Vec<ModelEntry>, LemonadeError> {
    let resp = self
      .http
      .get(format!("{}/api/v1/models", self.base))
      .send()
      .await
      .map_err(transport)?;
    ensure_success(&resp)?;
    let list: ModelList = decode(resp).await?;
    Ok(list.data)
  }

  /// `POST /api/v1/load {model_name}` — preload a model into memory.
  /// Optional (chat autoloads); use it to avoid first-request latency.
  pub async fn load(&self, model_name: &str) -> Result<(), LemonadeError> {
    self.load_with(model_name, &LoadOptions::default()).await
  }

  /// `POST /api/v1/load` with the launch options lemond honors beyond
  /// the model name. See [`LoadOptions`].
  pub async fn load_with(&self, model_name: &str, opts: &LoadOptions) -> Result<(), LemonadeError> {
    self
      .post_model_action("load", load_body(model_name, opts))
      .await
  }

  /// `POST /api/v1/unload {model_name}` — unload a model from memory.
  pub async fn unload(&self, model_name: &str) -> Result<(), LemonadeError> {
    self
      .post_model_action("unload", serde_json::json!({ "model_name": model_name }))
      .await
  }

  async fn post_model_action(
    &self,
    action: &str,
    body: serde_json::Value,
  ) -> Result<(), LemonadeError> {
    let resp = self
      .http
      .post(format!("{}/api/v1/{action}", self.base))
      .json(&body)
      .send()
      .await
      .map_err(transport)?;
    ensure_success(&resp)?;
    let parsed: StatusResponse = decode(resp).await?;
    if parsed.status.eq_ignore_ascii_case("success") {
      Ok(())
    } else {
      Err(LemonadeError::Api(if parsed.message.is_empty() {
        format!("{action} returned status {:?}", parsed.status)
      } else {
        parsed.message
      }))
    }
  }
}

fn transport(e: reqwest::Error) -> LemonadeError {
  LemonadeError::Transport(e.to_string())
}

fn ensure_success(resp: &reqwest::Response) -> Result<(), LemonadeError> {
  let status = resp.status();
  if status.is_success() {
    Ok(())
  } else {
    Err(LemonadeError::Http {
      status: status.as_u16(),
    })
  }
}

async fn decode<T: serde::de::DeserializeOwned>(
  resp: reqwest::Response,
) -> Result<T, LemonadeError> {
  resp
    .json::<T>()
    .await
    .map_err(|e| LemonadeError::Decode(e.to_string()))
}

#[cfg(test)]
mod tests {
  use super::*;
  use tokio::io::{AsyncReadExt, AsyncWriteExt};
  use tokio::net::TcpListener;
  use tokio::sync::mpsc;

  /// One captured request: the first request line (`"POST /api/v1/load HTTP/1.1"`)
  /// and the raw body bytes, so tests can assert method, path, and payload.
  #[derive(Debug)]
  struct CapturedRequest {
    line: String,
    body: String,
  }

  /// Spawn a loopback fake `lemond` that, for every connection, replies
  /// with `status` + `body` and forwards the request line + body over a
  /// channel. Loops until the listener is dropped (task end).
  async fn spawn_fake(
    status: u16,
    resp_body: &'static str,
  ) -> (u16, mpsc::UnboundedReceiver<CapturedRequest>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
      loop {
        let Ok((mut sock, _)) = listener.accept().await else {
          break;
        };
        let mut buf = vec![0u8; 4096];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        let raw = String::from_utf8_lossy(&buf[..n]).into_owned();
        let line = raw.lines().next().unwrap_or_default().to_string();
        let req_body = raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();
        let _ = tx.send(CapturedRequest {
          line,
          body: req_body,
        });
        let resp = format!(
          "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
          resp_body.len()
        );
        let _ = sock.write_all(resp.as_bytes()).await;
      }
    });
    (port, rx)
  }

  #[tokio::test]
  async fn list_models_parses_openai_list() {
    let (port, _rx) = spawn_fake(
      200,
      r#"{"object":"list","data":[{"id":"Qwen2.5-7B"},{"id":"Llama-3.1-8B"}]}"#,
    )
    .await;
    let client = LemonadeClient::new(port).unwrap();
    let models = client.list_models().await.unwrap();
    assert_eq!(models, vec!["Qwen2.5-7B", "Llama-3.1-8B"]);
  }

  #[tokio::test]
  async fn load_posts_model_name_and_parses_success() {
    let (port, mut rx) =
      spawn_fake(200, r#"{"status":"success","message":"Loaded model: M"}"#).await;
    let client = LemonadeClient::new(port).unwrap();
    client.load("Qwen2.5-7B").await.unwrap();
    let req = rx.recv().await.expect("fake saw a request");
    assert_eq!(req.line, "POST /api/v1/load HTTP/1.1");
    assert_eq!(req.body, r#"{"model_name":"Qwen2.5-7B"}"#);
  }

  #[tokio::test]
  async fn load_with_sends_ctx_size_and_recipe_args() {
    let (port, mut rx) =
      spawn_fake(200, r#"{"status":"success","message":"Loaded model: M"}"#).await;
    let client = LemonadeClient::new(port).unwrap();
    client
      .load_with(
        "qwen3.5-4b-FLM",
        &LoadOptions {
          ctx_size: Some(8192),
          recipe_args: Some(("flm_args".to_string(), "--foo bar".to_string())),
        },
      )
      .await
      .unwrap();
    let req = rx.recv().await.expect("fake saw a request");
    assert_eq!(req.line, "POST /api/v1/load HTTP/1.1");
    let body: serde_json::Value = serde_json::from_str(&req.body).expect("json body");
    assert_eq!(body["model_name"], "qwen3.5-4b-FLM");
    assert_eq!(body["ctx_size"], 8192);
    assert_eq!(body["flm_args"], "--foo bar");
  }

  #[test]
  fn load_body_omits_unset_options() {
    // Default options must keep the legacy minimal body — lemond treats an
    // explicit field as an override worth persisting/merging.
    let body = load_body("M", &LoadOptions::default());
    assert_eq!(body, serde_json::json!({"model_name": "M"}));
  }

  #[tokio::test]
  async fn health_parses_loaded_model() {
    let (port, _rx) = spawn_fake(200, r#"{"status":"ok","model_loaded":"Qwen2.5-7B"}"#).await;
    let client = LemonadeClient::new(port).unwrap();
    let health = client.health().await.unwrap();
    assert_eq!(health.status, "ok");
    assert_eq!(health.model_loaded.as_deref(), Some("Qwen2.5-7B"));
  }

  #[tokio::test]
  async fn live_ok_on_200() {
    let (port, _rx) = spawn_fake(200, "").await;
    let client = LemonadeClient::new(port).unwrap();
    client.live().await.unwrap();
  }

  #[tokio::test]
  async fn non_2xx_is_http_error() {
    let (port, _rx) = spawn_fake(500, r#"{"detail":"boom"}"#).await;
    let client = LemonadeClient::new(port).unwrap();
    let err = client.list_models().await.unwrap_err();
    assert!(
      matches!(err, LemonadeError::Http { status: 500 }),
      "got {err:?}"
    );
  }

  #[tokio::test]
  async fn malformed_json_is_decode_error() {
    let (port, _rx) = spawn_fake(200, "not json at all").await;
    let client = LemonadeClient::new(port).unwrap();
    let err = client.list_models().await.unwrap_err();
    assert!(matches!(err, LemonadeError::Decode(_)), "got {err:?}");
  }

  #[tokio::test]
  async fn connection_refused_is_transport_error() {
    // Nothing listening on this port → connect refused, distinct from Http.
    let client = LemonadeClient::new(1).unwrap();
    let err = client.live().await.unwrap_err();
    assert!(matches!(err, LemonadeError::Transport(_)), "got {err:?}");
  }

  #[tokio::test]
  async fn load_logical_failure_is_api_error() {
    let (port, _rx) = spawn_fake(200, r#"{"status":"error","message":"no such model"}"#).await;
    let client = LemonadeClient::new(port).unwrap();
    let err = client.load("nope").await.unwrap_err();
    assert!(
      matches!(&err, LemonadeError::Api(m) if m.contains("no such model")),
      "got {err:?}"
    );
  }
}
