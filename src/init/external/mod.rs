//! Init wizard's external-tool config patchers.
//!
//! Each module under `tools/` implements [`ToolPatcher`] for one
//! supported AI dev tool (OpenCode, Aider, Continue, Zed, pi.dev,
//! plus the `env.sh` shell-env writer). The wizard's
//! `run_integrations_step` presents a cliclack multiselect, then
//! calls [`dry_run`] / [`apply`] per chosen patcher.
//!
//! Shared with the llamastash-own config writer:
//! - redaction allowlist + diff rendering from
//!   [`crate::util::config_patch`]
//! - atomic write primitive from [`crate::util::atomic_write`]
//!
//! Per-tool modules only declare: path, format, and the additions
//! `serde_json::Value` to merge in. The merge / read-current /
//! diff / redact / atomic-write plumbing all lives here so a new
//! tool is ~30 lines.

pub mod merge;
pub mod tools;
pub mod write;

use std::path::PathBuf;

use serde::Serialize;

use crate::util::config_patch::{redact_diff, render_human, RedactedDiffEntry};

/// Serialisation format the patcher's target file uses on disk. We
/// always model the in-memory additions as `serde_json::Value`
/// (JSON is the lowest common denominator); the YAML variant goes
/// through `yaml_serde::to_string` at write time. Reading the
/// current file does the reverse for YAML.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
  Json,
  Yaml,
  /// Patcher manages the whole file body itself — bypasses merge
  /// (used by [`tools::env_sh`] which writes a shell script, not
  /// a merge-target config).
  Raw,
}

/// Inputs every patcher gets when building its additions.
///
/// `proxy_base_url` is the OpenAI-compat endpoint llamastash serves
/// (e.g. `http://127.0.0.1:11435/v1`); each tool's `baseURL` /
/// `api_url` / `apiBase` / `openai-api-base` field maps to it
/// verbatim. `api_key` is a non-secret stub — llama.cpp's
/// llama-server ignores Authorization; we still set one so clients
/// that *require* a non-empty key in their config don't refuse to
/// boot.
///
/// `is_embed`: the model is an embedding model (nomic-embed,
/// snowflake-arctic-embed, etc.). Patchers that care about the
/// distinction — Continue.dev's `roles` field, pi.dev's `api`
/// field — branch on this. Tools that don't differentiate just
/// register the model and let the user wire it up.
#[derive(Debug, Clone)]
pub struct PatchContext {
  pub proxy_base_url: String,
  pub api_key: String,
  pub model_id: Option<String>,
  pub is_embed: bool,
}

/// One supported external-tool patcher. Implementors declare where
/// the tool's config lives, what format it uses, and the JSON
/// additions to merge in.
pub trait ToolPatcher: Send + Sync {
  /// Short stable identifier — used for `--integrations <id>,...`
  /// and for the `tool_id` field on dry-run / apply outcomes.
  fn id(&self) -> &'static str;
  /// Human-readable label for the picker.
  fn display_name(&self) -> &'static str;
  /// Canonical on-disk path for a fresh install. `None` when the
  /// home directory can't be resolved (headless CI without `$HOME`);
  /// the caller surfaces that as [`PatchError::NoHome`].
  fn default_path(&self) -> Option<PathBuf>;
  /// Additional paths to check for an *existing* config before
  /// falling back to [`Self::default_path`]. Returned in priority order:
  /// the first path that actually exists wins. Default impl is
  /// empty — tools that accept multiple filename variants (OpenCode's
  /// `.jsonc` / `.json`, Continue's `.yaml` / `.yml`) override this
  /// to enumerate them, so re-running `init` patches the user's
  /// existing file rather than creating a parallel one.
  fn alt_paths(&self) -> Vec<PathBuf> {
    Vec::new()
  }
  fn format(&self) -> Format;
  /// Build the additions blob to merge into the existing file. For
  /// [`Format::Raw`] patchers this is ignored (the patcher
  /// overrides [`Self::raw_body`] instead).
  fn build_additions(&self, ctx: &PatchContext) -> serde_json::Value;
  /// Override the default object-recursive merge. Default impl is
  /// `merge::merge(current, build_additions(ctx))` (objects recurse,
  /// arrays replace wholesale).
  ///
  /// Tools whose schema includes arrays of named objects — Continue
  /// (`models[]`), Zed (`available_models[]`), pi.dev (`models[]`)
  /// — override this to merge those arrays *by name* so re-running
  /// `init` doesn't wipe model entries the user added manually.
  fn merge_with_current(
    &self,
    current: serde_json::Value,
    ctx: &PatchContext,
  ) -> serde_json::Value {
    merge::merge(current, self.build_additions(ctx))
  }
  /// For [`Format::Raw`] patchers: produce the full file body to
  /// write. Default implementation returns `None`, which means
  /// merge-based writes are used (Json/Yaml).
  fn raw_body(&self, _ctx: &PatchContext) -> Option<String> {
    None
  }
  /// Unix mode for the on-disk file. Defaults to `0o600` — these
  /// files may carry user-controllable api-key stubs and live in
  /// `$HOME`; group/world read isn't useful. Tools that prefer
  /// 0o644 can override (e.g. [`tools::env_sh`] which contains no
  /// secrets and may be sourced from group shells).
  fn unix_mode(&self) -> u32 {
    0o600
  }
}

/// Preview-only outcome — bytes never hit disk. Returned by
/// [`dry_run`] and embedded in `init --json` so an agent can show
/// the user what would change before they consent.
#[derive(Debug, Clone, Serialize)]
pub struct DryRunOutcome {
  pub tool_id: &'static str,
  pub display_name: &'static str,
  pub path: PathBuf,
  pub diff_human: String,
  pub diff_json: Vec<RedactedDiffEntry>,
}

/// Result of a successful [`apply`]. `written_bytes` is the size of
/// the final file (post-merge); `diff_*` is the redacted view of
/// what changed, identical to the corresponding [`DryRunOutcome`].
#[derive(Debug, Clone, Serialize)]
pub struct ApplyOutcome {
  pub tool_id: &'static str,
  pub display_name: &'static str,
  pub path: PathBuf,
  pub written_bytes: u64,
  pub diff_human: String,
  pub diff_json: Vec<RedactedDiffEntry>,
}

#[derive(Debug, thiserror::Error)]
pub enum PatchError {
  #[error("no home directory available; cannot resolve {tool_id} default path")]
  NoHome { tool_id: &'static str },
  #[error("{tool_id}: read {}: {error}", path.display())]
  Read {
    tool_id: &'static str,
    path: PathBuf,
    error: String,
  },
  #[error("{tool_id}: parse {} ({format:?}): {error}", path.display())]
  Parse {
    tool_id: &'static str,
    path: PathBuf,
    format: Format,
    error: String,
  },
  #[error("serialise additions: {0}")]
  Serialise(String),
  #[error("{tool_id}: write {}: {error}", path.display())]
  Write {
    tool_id: &'static str,
    path: PathBuf,
    error: String,
  },
}

/// Compute the redacted diff that [`apply`] *would* write, without
/// touching the filesystem. `override_path` lets tests target a
/// tempdir; production callers pass `None` to use the tool's
/// default location.
pub fn dry_run(
  patcher: &dyn ToolPatcher,
  ctx: &PatchContext,
  override_path: Option<PathBuf>,
) -> Result<DryRunOutcome, PatchError> {
  let path = resolve_path(patcher, override_path)?;
  let diff_entries = match patcher.format() {
    Format::Json | Format::Yaml => write::compute_diff(patcher, ctx, &path, patcher.format())?,
    Format::Raw => write::compute_raw_diff(patcher, ctx, &path)?,
  };
  let diff_json = redact_diff(&diff_entries);
  let diff_human = render_human(&diff_json);
  Ok(DryRunOutcome {
    tool_id: patcher.id(),
    display_name: patcher.display_name(),
    path,
    diff_human,
    diff_json,
  })
}

/// Apply the patch: read current, merge additions, write atomic.
/// Returns the redacted diff alongside `written_bytes` so the
/// wizard's summary can render it without re-reading the file.
pub fn apply(
  patcher: &dyn ToolPatcher,
  ctx: &PatchContext,
  override_path: Option<PathBuf>,
) -> Result<ApplyOutcome, PatchError> {
  let path = resolve_path(patcher, override_path)?;
  let (diff_entries, written_bytes) = match patcher.format() {
    Format::Json | Format::Yaml => write::apply_merge(patcher, ctx, &path, patcher.format())?,
    Format::Raw => write::apply_raw(patcher, ctx, &path)?,
  };
  let diff_json = redact_diff(&diff_entries);
  let diff_human = render_human(&diff_json);
  Ok(ApplyOutcome {
    tool_id: patcher.id(),
    display_name: patcher.display_name(),
    path,
    written_bytes,
    diff_human,
    diff_json,
  })
}

fn resolve_path(
  patcher: &dyn ToolPatcher,
  override_path: Option<PathBuf>,
) -> Result<PathBuf, PatchError> {
  if let Some(p) = override_path {
    return Ok(p);
  }
  let default = patcher.default_path().ok_or(PatchError::NoHome {
    tool_id: patcher.id(),
  })?;
  // Prefer an existing alt path (e.g. opencode.jsonc when the user
  // edits theirs with comments) over creating a parallel canonical
  // file. Falls back to the default for fresh installs.
  for alt in patcher.alt_paths() {
    if alt.exists() {
      return Ok(alt);
    }
  }
  Ok(default)
}

/// Returns every patcher the wizard knows about. Order is the
/// order the picker displays.
pub fn all_patchers() -> Vec<Box<dyn ToolPatcher>> {
  tools::registered()
}

/// Resolve a patcher by its [`ToolPatcher::id`]. Used by the wizard's
/// `--integrations <id>,...` non-interactive form.
pub fn patcher_by_id(id: &str) -> Option<Box<dyn ToolPatcher>> {
  all_patchers().into_iter().find(|p| p.id() == id)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Trivial test patcher used by the skeleton's own tests. Not
  /// registered with [`all_patchers`].
  struct StubJson;

  impl ToolPatcher for StubJson {
    fn id(&self) -> &'static str {
      "stub-json"
    }
    fn display_name(&self) -> &'static str {
      "Stub JSON"
    }
    fn default_path(&self) -> Option<PathBuf> {
      None
    }
    fn format(&self) -> Format {
      Format::Json
    }
    fn build_additions(&self, ctx: &PatchContext) -> serde_json::Value {
      serde_json::json!({
        "providers": {
          "llamastash": {
            "baseURL": ctx.proxy_base_url,
            "apiKey": ctx.api_key,
          }
        }
      })
    }
  }

  fn ctx() -> PatchContext {
    PatchContext {
      proxy_base_url: "http://127.0.0.1:11435/v1".into(),
      api_key: "llamastash".into(),
      model_id: None,
      is_embed: false,
    }
  }

  #[test]
  fn dry_run_against_missing_file_reports_additions() {
    let dir = crate::util::test_temp::unique_temp_dir("ext-skeleton-dry");
    let path = dir.join("stub.json");
    let out = dry_run(&StubJson, &ctx(), Some(path.clone())).expect("dry_run");
    assert_eq!(out.tool_id, "stub-json");
    assert_eq!(out.path, path);
    // Whole-subtree Added rows collapse to the top-level new key —
    // same behaviour as the YAML writer (see config::writer::walk_diff).
    let added = out
      .diff_json
      .iter()
      .find(|d| d.path == "providers")
      .expect("providers added row");
    assert!(added.value_yaml.contains("baseURL"));
    assert!(!path.exists(), "dry_run never touches disk");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn dry_run_into_existing_file_reports_only_leaf_changes() {
    let dir = crate::util::test_temp::unique_temp_dir("ext-skeleton-existing");
    let path = dir.join("stub.json");
    // Existing file already has the providers tree but a stale baseURL.
    std::fs::write(
      &path,
      r#"{"providers":{"llamastash":{"baseURL":"http://old/v1","apiKey":"llamastash"}}}"#,
    )
    .unwrap();
    let out = dry_run(&StubJson, &ctx(), Some(path.clone())).expect("dry_run");
    let leaf = out
      .diff_json
      .iter()
      .find(|d| d.path == "providers.llamastash.baseURL")
      .expect("changed leaf");
    assert_eq!(leaf.kind, "changed");
    assert!(leaf.value_yaml.contains("http://127.0.0.1:11435/v1"));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn apply_then_apply_is_idempotent_on_same_inputs() {
    let dir = crate::util::test_temp::unique_temp_dir("ext-skeleton-apply");
    let path = dir.join("stub.json");
    let first = apply(&StubJson, &ctx(), Some(path.clone())).expect("first apply");
    assert!(first.written_bytes > 0);
    let second = apply(&StubJson, &ctx(), Some(path.clone())).expect("second apply");
    // No changes the second time around.
    assert!(second.diff_json.is_empty(), "idempotent");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn no_home_when_default_path_unresolvable_and_no_override() {
    let err = dry_run(&StubJson, &ctx(), None).unwrap_err();
    assert!(matches!(
      err,
      PatchError::NoHome {
        tool_id: "stub-json"
      }
    ));
  }

  /// Patcher that exposes a default + one alt path. The alt is
  /// checked for existence first; the default is the fresh-install
  /// fallback.
  struct WithAlt {
    default: PathBuf,
    alt: PathBuf,
  }

  impl ToolPatcher for WithAlt {
    fn id(&self) -> &'static str {
      "with-alt"
    }
    fn display_name(&self) -> &'static str {
      "WithAlt"
    }
    fn default_path(&self) -> Option<PathBuf> {
      Some(self.default.clone())
    }
    fn alt_paths(&self) -> Vec<PathBuf> {
      vec![self.alt.clone()]
    }
    fn format(&self) -> Format {
      Format::Json
    }
    fn build_additions(&self, _ctx: &PatchContext) -> serde_json::Value {
      serde_json::json!({ "k": "v" })
    }
  }

  #[test]
  fn resolve_path_prefers_existing_alt_over_default() {
    let dir = crate::util::test_temp::unique_temp_dir("resolve-alt");
    let patcher = WithAlt {
      default: dir.join("default.json"),
      alt: dir.join("alt.jsonc"),
    };
    std::fs::write(&patcher.alt, "{}").unwrap();
    let resolved = resolve_path(&patcher, None).unwrap();
    assert_eq!(resolved, patcher.alt);
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn resolve_path_falls_back_to_default_when_no_alt_exists() {
    let dir = crate::util::test_temp::unique_temp_dir("resolve-default");
    let patcher = WithAlt {
      default: dir.join("default.json"),
      alt: dir.join("alt.jsonc"),
    };
    let resolved = resolve_path(&patcher, None).unwrap();
    assert_eq!(resolved, patcher.default);
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn resolve_path_override_wins_over_alt_and_default() {
    let dir = crate::util::test_temp::unique_temp_dir("resolve-override");
    let patcher = WithAlt {
      default: dir.join("default.json"),
      alt: dir.join("alt.jsonc"),
    };
    std::fs::write(&patcher.alt, "{}").unwrap();
    let explicit = dir.join("explicit.json");
    let resolved = resolve_path(&patcher, Some(explicit.clone())).unwrap();
    assert_eq!(resolved, explicit);
    std::fs::remove_dir_all(&dir).ok();
  }
}
