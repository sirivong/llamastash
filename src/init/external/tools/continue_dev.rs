//! Continue.dev patcher — `~/.continue/config.yaml`.
//!
//! Continue's schema is `models: [ { name, provider, apiBase, model,
//! roles }, ... ]` — a TOP-LEVEL array of named entries. Wholesale
//! replacing it would clobber user-added OpenAI/Anthropic models, so
//! [`merge_with_current`](ToolPatcher::merge_with_current) is
//! overridden to merge by `name`: any entry already named
//! `llamastash` is replaced, everything else is left untouched.
//!
//! `config.yaml` is the current format (per Continue's docs as of
//! 2025–2026 — `config.json` is deprecated; we never write the old
//! format).

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::init::external::{merge, Format, PatchContext, ToolPatcher};

pub struct ContinueDev;

const LLAMASTASH_MODEL_NAME: &str = "llamastash";

impl ToolPatcher for ContinueDev {
  fn id(&self) -> &'static str {
    "continue"
  }
  fn display_name(&self) -> &'static str {
    "Continue.dev"
  }
  fn default_path(&self) -> Option<PathBuf> {
    crate::util::paths::home_dir().map(|h| h.join(".continue").join("config.yaml"))
  }
  fn alt_paths(&self) -> Vec<PathBuf> {
    // Some users name their YAML `.yml`; check that variant before
    // creating a parallel `.yaml`. We deliberately do NOT detect the
    // deprecated `config.json` here — Continue is migrating off it,
    // and writing to it would silently keep users on the old format.
    crate::util::paths::home_dir()
      .map(|h| vec![h.join(".continue").join("config.yml")])
      .unwrap_or_default()
  }
  fn format(&self) -> Format {
    Format::Yaml
  }
  fn build_additions(&self, ctx: &PatchContext) -> Value {
    let model_id = ctx.model_id.as_deref().unwrap_or("default");
    // Continue's `roles` enum drives what the IDE attempts with the
    // model. Setting `chat`/`edit` on an embedder (nomic-embed-text
    // etc.) gives confusing errors because Continue tries to chat
    // with an encoder-only model. The `embed` role is the right
    // wire for embedding models.
    let roles = if ctx.is_embed {
      json!(["embed"])
    } else {
      json!(["chat", "edit"])
    };
    let mut entry = serde_json::Map::new();
    entry.insert("name".into(), json!(LLAMASTASH_MODEL_NAME));
    entry.insert("provider".into(), json!("openai"));
    entry.insert("apiBase".into(), json!(ctx.proxy_base_url));
    entry.insert("model".into(), json!(model_id));
    entry.insert("apiKey".into(), json!(ctx.api_key));
    entry.insert("roles".into(), roles);
    json!({
      "name": "llamastash",
      "version": "1.0.0",
      "schema": "v1",
      "models": [Value::Object(entry)],
    })
  }
  fn merge_with_current(&self, current: Value, ctx: &PatchContext) -> Value {
    let additions = self.build_additions(ctx);
    let Value::Object(mut additions_obj) = additions else {
      return merge::merge(current, additions);
    };
    // Pull our `models` array out of the additions first — we'll
    // splice it into the *existing* models[] by name rather than let
    // the recursive merge replace the whole array.
    let our_models = additions_obj
      .remove("models")
      .and_then(|v| match v {
        Value::Array(a) => Some(a),
        _ => None,
      })
      .unwrap_or_default();
    // Top-level metadata (`name`, `version`, `schema`) is "fill in
    // only if absent" — the user owns these. Default recursive merge
    // would override the user's `name: MyConfig` with our
    // `name: llamastash` placeholder, which is user-hostile.
    let current_obj = match current {
      Value::Object(m) => m,
      _ => serde_json::Map::new(),
    };
    for key in ["name", "version", "schema"] {
      if current_obj.contains_key(key) {
        additions_obj.remove(key);
      }
    }
    // Anything still in additions_obj is safe to merge — empty in
    // practice today (we've removed everything), but the recursion
    // is cheap and future-proofs against added fields.
    let mut merged = merge::merge(Value::Object(current_obj), Value::Object(additions_obj));
    if !our_models.is_empty() {
      if let Value::Object(ref mut m) = merged {
        let slot = m
          .entry("models")
          .or_insert_with(|| Value::Array(Vec::new()));
        if let Value::Array(arr) = slot {
          splice_named(arr, our_models, "name");
        }
      }
    }
    merged
  }
}

/// Splice `incoming` entries into `current` by `name_field`: replace
/// matching entries, append new ones. Generic enough that
/// pi.dev / Zed could reuse it if their `available_models` ever
/// needs the same behaviour (today their schema means our entries
/// own the array, so wholesale replace is fine for those).
fn splice_named(current: &mut Vec<Value>, incoming: Vec<Value>, name_field: &str) {
  for new_entry in incoming {
    let key = new_entry
      .get(name_field)
      .and_then(|v| v.as_str())
      .map(String::from);
    let Some(key) = key else {
      current.push(new_entry);
      continue;
    };
    let pos = current
      .iter()
      .position(|c| c.get(name_field).and_then(|v| v.as_str()) == Some(key.as_str()));
    match pos {
      Some(i) => current[i] = new_entry,
      None => current.push(new_entry),
    }
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

  fn embed_ctx() -> PatchContext {
    PatchContext {
      proxy_base_url: "http://127.0.0.1:11435/v1".into(),
      api_key: "llamastash".into(),
      model_id: Some("nomic-embed-text-v1.5".into()),
      is_embed: true,
    }
  }

  #[test]
  fn writes_models_array_into_empty_file() {
    let dir = crate::util::test_temp::unique_temp_dir("continue-empty");
    let path = dir.join("config.yaml");
    apply(&ContinueDev, &ctx(), Some(path.clone())).expect("apply");
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("name: llamastash"));
    assert!(body.contains("apiBase: http://127.0.0.1:11435/v1"));
    assert!(body.contains("model: qwen3-coder-30b"));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn preserves_user_models_in_array() {
    let dir = crate::util::test_temp::unique_temp_dir("continue-user-models");
    let path = dir.join("config.yaml");
    std::fs::write(
      &path,
      "name: My Config\nversion: 1.0.0\nschema: v1\nmodels:\n  - name: GPT-4\n    provider: openai\n    model: gpt-4o\n",
    )
    .unwrap();
    apply(&ContinueDev, &ctx(), Some(path.clone())).expect("apply");
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("name: GPT-4"), "user model preserved");
    assert!(body.contains("name: llamastash"), "our model added");
    assert!(body.contains("name: My Config"), "top-level name preserved");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn re_applying_replaces_only_llamastash_entry() {
    let dir = crate::util::test_temp::unique_temp_dir("continue-reapply");
    let path = dir.join("config.yaml");
    // User has GPT-4 + an older llamastash entry pointing at port 11434.
    std::fs::write(
      &path,
      "name: cfg\nversion: 1.0.0\nschema: v1\nmodels:\n  - name: GPT-4\n    provider: openai\n    model: gpt-4o\n  - name: llamastash\n    provider: openai\n    apiBase: http://127.0.0.1:11434/v1\n    model: old\n",
    )
    .unwrap();
    apply(&ContinueDev, &ctx(), Some(path.clone())).expect("apply");
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("name: GPT-4"));
    assert!(body.contains("apiBase: http://127.0.0.1:11435/v1"));
    assert!(!body.contains("11434"), "old llamastash entry replaced");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn embed_model_writes_embed_role_not_chat_edit() {
    let dir = crate::util::test_temp::unique_temp_dir("continue-embed");
    let path = dir.join("config.yaml");
    apply(&ContinueDev, &embed_ctx(), Some(path.clone())).expect("apply");
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("- embed"), "embed role written");
    assert!(
      !body.contains("- chat"),
      "chat role NOT written for embedder"
    );
    assert!(
      !body.contains("- edit"),
      "edit role NOT written for embedder"
    );
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn idempotent_second_apply_no_diff() {
    let dir = crate::util::test_temp::unique_temp_dir("continue-idem");
    let path = dir.join("config.yaml");
    apply(&ContinueDev, &ctx(), Some(path.clone())).expect("first");
    let second = apply(&ContinueDev, &ctx(), Some(path.clone())).expect("second");
    assert!(second.diff_json.is_empty(), "idempotent");
    std::fs::remove_dir_all(&dir).ok();
  }
}
