//! pi.dev patcher — `~/.pi/agent/models.json`.
//!
//! Schema: `providers.<id>` with `baseUrl`, `api: "openai-completions"`,
//! `apiKey: "$ENV_VAR"`, and a `models[]` array. Per pi.dev docs,
//! the `apiKey` field accepts `$ENV_VAR` references (and `!command`
//! shell-out) — we use the env reference so the literal stub
//! never lands on disk.
//!
//! The `models[]` array is inside our own `llamastash` provider
//! block, so a wholesale replace only touches our entries — the
//! default object-recursive merge is fine.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::init::external::{Format, PatchContext, ToolPatcher};

pub struct PiDev;

impl ToolPatcher for PiDev {
  fn id(&self) -> &'static str {
    "pi"
  }
  fn display_name(&self) -> &'static str {
    "pi.dev"
  }
  fn default_path(&self) -> Option<PathBuf> {
    crate::util::paths::home_dir().map(|h| h.join(".pi").join("agent").join("models.json"))
  }
  fn format(&self) -> Format {
    Format::Json
  }
  fn build_additions(&self, ctx: &PatchContext) -> Value {
    let mut models = Vec::new();
    if let Some(id) = &ctx.model_id {
      models.push(json!({
        "id": id,
        "name": id,
        "contextWindow": 32768,
        "maxTokens": 8192,
      }));
    }
    // pi.dev's `api` field distinguishes chat completions from
    // embedding endpoints. Setting `openai-completions` on an
    // embedder routes the request to the wrong endpoint shape.
    let api_kind = if ctx.is_embed {
      "openai-embeddings"
    } else {
      "openai-completions"
    };
    json!({
      "providers": {
        "llamastash": {
          "baseUrl": ctx.proxy_base_url,
          "api": api_kind,
          "apiKey": "$LLAMASTASH_API_KEY",
          "models": models,
        }
      }
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::init::external::apply;

  fn ctx() -> PatchContext {
    PatchContext {
      proxy_base_url: "http://127.0.0.1:11435/v1".into(),
      api_key: "llamastash".into(),
      model_id: Some("qwen3-coder-30b".into()),
      is_embed: false,
    }
  }

  #[test]
  fn writes_provider_block_into_empty_file() {
    let dir = crate::util::test_temp::unique_temp_dir("pi-empty");
    let path = dir.join("models.json");
    apply(&PiDev, &ctx(), Some(path.clone())).expect("apply");
    let body: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
      body["providers"]["llamastash"]["baseUrl"],
      "http://127.0.0.1:11435/v1"
    );
    assert_eq!(body["providers"]["llamastash"]["api"], "openai-completions");
    assert_eq!(
      body["providers"]["llamastash"]["models"][0]["id"],
      "qwen3-coder-30b"
    );
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn preserves_user_providers_alongside_llamastash() {
    let dir = crate::util::test_temp::unique_temp_dir("pi-coexist");
    let path = dir.join("models.json");
    std::fs::write(
      &path,
      r#"{"providers":{"openai":{"baseUrl":"https://api.openai.com/v1","api":"openai-completions"}}}"#,
    )
    .unwrap();
    apply(&PiDev, &ctx(), Some(path.clone())).expect("apply");
    let body: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
      body["providers"]["openai"]["baseUrl"],
      "https://api.openai.com/v1"
    );
    assert!(body["providers"]["llamastash"].is_object());
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn api_key_renders_as_env_reference() {
    let v = PiDev.build_additions(&ctx());
    assert_eq!(
      v["providers"]["llamastash"]["apiKey"],
      "$LLAMASTASH_API_KEY"
    );
  }

  #[test]
  fn embed_model_flips_api_to_openai_embeddings() {
    let mut c = ctx();
    c.is_embed = true;
    c.model_id = Some("nomic-embed-text-v1.5".into());
    let v = PiDev.build_additions(&c);
    assert_eq!(v["providers"]["llamastash"]["api"], "openai-embeddings");
  }

  #[test]
  fn chat_model_keeps_openai_completions() {
    let v = PiDev.build_additions(&ctx());
    assert_eq!(v["providers"]["llamastash"]["api"], "openai-completions");
  }
}
