//! Zed patcher — `~/.config/zed/settings.json`.
//!
//! Schema: `language_models.openai_compatible.<display_name>` with
//! `api_url` + `available_models[]`. Per Zed's docs, API keys live
//! in env (`<DISPLAY_NAME_UPPERCASE>_API_KEY` — i.e.
//! `LLAMASTASH_API_KEY`) and are *not* stored in `settings.json`.
//! We honour that — only the URL and the model list land on disk.
//!
//! `available_models[]` is inside our own `LlamaStash` block so a
//! wholesale array replace only touches entries we own. The default
//! object-recursive merge is fine here — no smart array splicing
//! needed (unlike Continue.dev where the array is at root).

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::init::external::{Format, PatchContext, ToolPatcher};

pub struct Zed;

impl ToolPatcher for Zed {
  fn id(&self) -> &'static str {
    "zed"
  }
  fn display_name(&self) -> &'static str {
    "Zed"
  }
  fn default_path(&self) -> Option<PathBuf> {
    crate::util::paths::home_dir().map(|h| h.join(".config").join("zed").join("settings.json"))
  }
  fn format(&self) -> Format {
    Format::Json
  }
  fn build_additions(&self, ctx: &PatchContext) -> Value {
    let mut available_models = Vec::new();
    if let Some(id) = &ctx.model_id {
      available_models.push(json!({
        "name": id,
        "display_name": id,
        "max_tokens": 32768,
        "capabilities": {
          "tools": true,
          "images": false,
          "parallel_tool_calls": false,
          "prompt_cache_key": false,
        }
      }));
    }
    json!({
      "language_models": {
        "openai_compatible": {
          "LlamaStash": {
            "api_url": ctx.proxy_base_url,
            "available_models": available_models,
          }
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
  fn writes_openai_compatible_block_into_empty_file() {
    let dir = crate::util::test_temp::unique_temp_dir("zed-empty");
    let path = dir.join("settings.json");
    apply(&Zed, &ctx(), Some(path.clone())).expect("apply");
    let body: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let llamastash = &body["language_models"]["openai_compatible"]["LlamaStash"];
    assert_eq!(llamastash["api_url"], "http://127.0.0.1:11435/v1");
    assert_eq!(llamastash["available_models"][0]["name"], "qwen3-coder-30b");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn preserves_user_zed_settings_outside_llm_block() {
    let dir = crate::util::test_temp::unique_temp_dir("zed-coexist");
    let path = dir.join("settings.json");
    std::fs::write(
      &path,
      r#"{"theme":"One Dark","ui_font_size":16,"language_models":{"anthropic":{"version":"1"}}}"#,
    )
    .unwrap();
    apply(&Zed, &ctx(), Some(path.clone())).expect("apply");
    let body: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(body["theme"], "One Dark");
    assert_eq!(body["ui_font_size"], 16);
    assert_eq!(body["language_models"]["anthropic"]["version"], "1");
    assert!(body["language_models"]["openai_compatible"]["LlamaStash"].is_object());
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn api_key_not_written_to_settings_per_zed_convention() {
    let v = Zed.build_additions(&ctx());
    let llamastash = &v["language_models"]["openai_compatible"]["LlamaStash"];
    assert!(
      llamastash.get("api_key").is_none(),
      "Zed reads the key from $LLAMASTASH_API_KEY env, not settings.json"
    );
  }
}
