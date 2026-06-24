//! Model-catalog row type and the fuzzy reference matcher.
//!
//! `clap`/IPC-free so both the CLI (`crate::cli::resolve`, which adds the
//! IPC fetch + `CliExit` mapping on top) and the HTTP proxy depend *down*
//! on one matcher instead of the proxy importing "up" into `cli`.
//!
//! Model references accept an absolute path (matched verbatim against the
//! canonical path), an exact file name, or a case-insensitive substring
//! of the file name or parent directory.

/// One row from `list_models`. Lean wrapper kept independent of the
/// catalog's internal `DiscoveredModel` shape so the resolver stays
/// transport-agnostic.
#[derive(Debug, Clone)]
pub struct CatalogRow {
  /// Canonical absolute path to the launchable file (or shard 1).
  pub path: String,
  /// Short BLAKE3-derived canonical id (8 hex chars). Optional
  /// because the daemon's catalog computes it lazily — pre-launch
  /// rows omit it.
  pub model_id: Option<String>,
  pub parent: String,
  pub source: String,
  pub arch: Option<String>,
  pub quant: Option<String>,
  pub native_ctx: Option<u64>,
  pub mode_hint: Option<String>,
  pub parameter_label: Option<String>,
  /// GGUF weights footprint (sum of per-tensor storage bytes). `None`
  /// when the file is metadata-only or the header parse failed. Used
  /// by `list_human` for the SIZE column.
  pub weights_bytes: Option<u64>,
  /// Source-supplied human label preferred over the path's basename
  /// when set. Currently populated only for Ollama rows, where the
  /// content-addressed blob filename (`sha256-<hex>`) is hostile to
  /// scanning by eye.
  pub display_label: Option<String>,
  pub parse_error: Option<String>,
  /// Sibling shard paths for split GGUFs. Empty for single-file
  /// models. `path` is always shard 1; this carries shards 2..N so
  /// callers (`show`, future size aggregators) can compute the
  /// on-disk total without re-scanning the parent dir.
  pub split_siblings: Vec<String>,
  /// `true` when the GGUF header carried a `tokenizer.chat_template`
  /// string. Surfacing the boolean (not the full template) keeps the
  /// `list_models` wire shape lean; the template body is large.
  pub has_chat_template: bool,
  /// `true` when the GGUF carried a reasoning hint. Mirrors the
  /// `metadata.has_reasoning_hint` field on `list_models`.
  pub has_reasoning_hint: bool,
  /// `tokenizer.ggml.model` from the GGUF header (`"llama"`, `"qwen2"`).
  pub tokenizer_kind: Option<String>,
  /// `general.parameter_count` — the raw count behind
  /// `parameter_label` (`"7B"` is derived from `7e9`).
  pub total_parameters: Option<u64>,
}

impl CatalogRow {
  /// Build a row carrying only the fields preset-key classification and
  /// the fuzzy matcher read: `path`, `display_label` (→ [`Self::name`]),
  /// and `arch`. The daemon projects its `DiscoveredModel` catalog into
  /// these for `effective_presets` without rebuilding the full
  /// `list_models` shape; every other field is left empty/`None`.
  pub fn for_resolution(path: String, display_label: Option<String>, arch: Option<String>) -> Self {
    Self {
      path,
      model_id: None,
      parent: String::new(),
      source: String::new(),
      arch,
      quant: None,
      native_ctx: None,
      mode_hint: None,
      parameter_label: None,
      weights_bytes: None,
      display_label,
      parse_error: None,
      split_siblings: Vec::new(),
      has_chat_template: false,
      has_reasoning_hint: false,
      tokenizer_kind: None,
      total_parameters: None,
    }
  }

  /// Friendly label for human matching and table rendering.
  /// `display_label` (Ollama's `<name>:<tag>`) wins when set; falls
  /// back to the path basename.
  pub fn name(&self) -> String {
    if let Some(label) = &self.display_label {
      return label.clone();
    }
    std::path::Path::new(&self.path)
      .file_name()
      .map(|s| s.to_string_lossy().into_owned())
      .unwrap_or_else(|| self.path.clone())
  }
}

/// Distinguishes the three resolver failure modes the HTTP proxy needs
/// to surface as distinct HTTP responses (and which the CLI folds
/// together into a single `MODEL_NOT_FOUND` exit).
#[derive(Debug, Clone)]
pub enum ResolveError {
  /// Reference was empty after trimming.
  Empty,
  /// Zero candidates matched the reference. Proxy emits 404
  /// `model_not_found`.
  None,
  /// More than one candidate matched. Proxy emits 400
  /// `ambiguous_model` with the candidate list in `matches`.
  Many(Vec<CatalogRow>),
}

/// Resolve a model reference, preserving the distinction between "zero
/// candidates" and "many candidates" so callers (the HTTP proxy emits
/// 404 vs 400 with `matches: [...]`) can branch without re-running the
/// substring matcher themselves. The CLI's `resolve_model` wraps this,
/// folding every failure into a single `MODEL_NOT_FOUND` exit.
///
/// Precedence: exact path → exact name → case-insensitive substring of
/// name or parent.
pub fn resolve_model_with_candidates(
  rows: &[CatalogRow],
  reference: &str,
) -> Result<CatalogRow, ResolveError> {
  let needle = reference.trim();
  if needle.is_empty() {
    return Err(ResolveError::Empty);
  }

  // Tier 1: exact path / exact name. A full canonical path is
  // unambiguous by construction.
  let exact_path: Vec<&CatalogRow> = rows.iter().filter(|r| r.path == needle).collect();
  if exact_path.len() == 1 {
    return Ok(exact_path[0].clone());
  }
  let exact_name: Vec<&CatalogRow> = rows.iter().filter(|r| r.name() == needle).collect();
  if exact_name.len() == 1 {
    return Ok(exact_name[0].clone());
  }

  // Tier 2: case-insensitive substring of name OR parent.
  let lower = needle.to_lowercase();
  let candidates: Vec<&CatalogRow> = rows
    .iter()
    .filter(|r| {
      r.name().to_lowercase().contains(&lower) || r.parent.to_lowercase().contains(&lower)
    })
    .collect();
  match candidates.len() {
    0 => Err(ResolveError::None),
    1 => Ok(candidates[0].clone()),
    _ => Err(ResolveError::Many(
      candidates.into_iter().cloned().collect(),
    )),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn row(path: &str, parent: &str) -> CatalogRow {
    CatalogRow {
      path: path.to_string(),
      model_id: None,
      parent: parent.to_string(),
      source: "user".to_string(),
      arch: Some("llama".to_string()),
      quant: Some("Q4_K".to_string()),
      native_ctx: Some(8192),
      mode_hint: Some("chat".to_string()),
      parameter_label: Some("7B".to_string()),
      weights_bytes: Some(4_200_000_000),
      display_label: None,
      parse_error: None,
      split_siblings: Vec::new(),
      has_chat_template: false,
      has_reasoning_hint: false,
      tokenizer_kind: None,
      total_parameters: None,
    }
  }

  #[test]
  fn with_candidates_returns_many_for_ambiguous() {
    let rows = vec![
      row("/m/qwen-coder-7b.gguf", "/m"),
      row("/m/qwen-coder-13b.gguf", "/m"),
    ];
    match resolve_model_with_candidates(&rows, "qwen-coder") {
      Err(ResolveError::Many(cands)) => assert_eq!(cands.len(), 2),
      other => panic!("expected Many(2); got {other:?}"),
    }
  }

  #[test]
  fn with_candidates_returns_none_for_unmatched() {
    let rows = vec![row("/m/llama.gguf", "/m")];
    match resolve_model_with_candidates(&rows, "phi") {
      Err(ResolveError::None) => {}
      other => panic!("expected None; got {other:?}"),
    }
  }

  #[test]
  fn with_candidates_returns_empty_for_blank_reference() {
    match resolve_model_with_candidates(&[], "   ") {
      Err(ResolveError::Empty) => {}
      other => panic!("expected Empty; got {other:?}"),
    }
  }

  #[test]
  fn exact_path_wins_over_substring_overlap() {
    let rows = vec![
      row("/m/qwen-coder-7b.gguf", "/m"),
      row("/m/qwen-coder-13b.gguf", "/m"),
    ];
    let pick = resolve_model_with_candidates(&rows, "/m/qwen-coder-7b.gguf").unwrap();
    assert_eq!(pick.path, "/m/qwen-coder-7b.gguf");
  }
}
