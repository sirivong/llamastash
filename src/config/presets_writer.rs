//! Comment-preserving writer for the `presets:` block of `config.yaml`.
//!
//! The read path parses `config.yaml` whole via `yaml_serde`. This module
//! is the *only* mutation path for presets: it edits exactly the one node
//! being changed and leaves every other comment and bit of formatting —
//! including a hand-authored arch preset — byte-for-byte intact.
//!
//! [`yamlpath`] locates the precise byte span of any node (it tracks
//! comments and formatting via tree-sitter); we splice the rendered preset
//! body into that span ourselves. We do *not* use `yamlpatch`'s value-add /
//! value-replace ops for writes — they re-indent nested map/sequence values
//! incorrectly (flattening `{auto: true}` and `extras: [...]` to the wrong
//! depth). Deletes do go through `yamlpatch`'s `Op::Remove`, which removes a
//! whole key cleanly. Bodies are rendered as compact JSON, which is valid
//! single-line YAML flow with correct typing/quoting (a numeric-looking
//! string like `"10000"` stays a string), so splicing is a one-line insert.
//!
//! Entries are keyed by preset **name** under an `entries:` map, so every
//! mutation targets one map key — never a sequence, which the tooling
//! cannot edit in place. The patched document is routed through the shared
//! atomic [`crate::util::atomic_write::write_secure`] for the same
//! symlink/parent-mode/0600 guards the whole-file [`crate::config::writer`]
//! uses.

use std::path::Path;

use serde::Serialize;
use yaml_serde::Value as YamlValue;
use yamlpatch::{apply_yaml_patches, Op, Patch};
use yamlpath::{Component, Document, FeatureKind, Route};

use crate::config::writer::{preflight, WriteError};

const PRESETS_KEY: &str = "presets";
const ENTRIES_KEY: &str = "entries";
/// One indentation step. Matches the project's 2-space YAML convention;
/// only used when *creating* a nesting level (existing levels keep their
/// own indentation, derived from the document).
const STEP: usize = 2;

/// Create or update the named preset `name` under `model_key` in
/// `config_path`, preserving every unrelated comment and bit of
/// formatting. `body` serialises to a flat YAML mapping (the preset's
/// `ctx` / `reasoning` / `extras` plus the flattened typed knobs); `null`
/// fields are dropped so the written entry only carries set values.
pub fn upsert_preset(
  config_path: &Path,
  model_key: &str,
  name: &str,
  body: &impl Serialize,
) -> Result<(), WriteError> {
  preflight(config_path)?;
  let source = read_source(config_path)?;
  let pruned = prune_nulls(serialise(body)?);
  // Compact JSON is valid single-line YAML flow with faithful typing and
  // quoting — the body collapses to one line, so the splice is trivial.
  let flow = serde_json::to_string(&pruned).map_err(|e| WriteError::Serialise(e.to_string()))?;
  let new_source = splice_upsert(&source, model_key, name, &flow)?;
  write(config_path, &new_source)
}

/// Remove the named preset `name` under `model_key` from `config_path`,
/// preserving unrelated comments/formatting. When `name` is the model
/// key's only entry the now-empty `entries:` (or the whole model key, if
/// it carried nothing but `entries:`) is pruned. Returns `false` when the
/// preset wasn't present, so nothing was written.
pub fn remove_preset(config_path: &Path, model_key: &str, name: &str) -> Result<bool, WriteError> {
  preflight(config_path)?;
  let source = read_source(config_path)?;
  if source.trim().is_empty() {
    return Ok(false);
  }
  let current = parse(&source, config_path)?;
  let model_node = current.get(PRESETS_KEY).and_then(|p| p.get(model_key));
  let entries_map = match model_node
    .and_then(|m| m.get(ENTRIES_KEY))
    .and_then(YamlValue::as_mapping)
  {
    Some(m) if m.contains_key(name) => m,
    _ => return Ok(false),
  };

  let last_entry = entries_map.len() == 1;
  // A model block that holds only `entries:` is pruned wholesale; one that
  // also carries a hand-authored `default:` keeps the key (only `entries:`
  // goes) so the user's default line survives.
  let model_only_entries = model_node
    .and_then(YamlValue::as_mapping)
    .is_some_and(|m| m.len() == 1);
  // When that prune would empty the whole `presets:` map, drop the
  // top-level key too rather than leaving a degenerate `presets:` (null).
  let presets_only_model = current
    .get(PRESETS_KEY)
    .and_then(YamlValue::as_mapping)
    .is_some_and(|m| m.len() == 1);

  let route = if last_entry && model_only_entries && presets_only_model {
    key_route(&[PRESETS_KEY])
  } else if last_entry && model_only_entries {
    key_route(&[PRESETS_KEY, model_key])
  } else if last_entry {
    key_route(&[PRESETS_KEY, model_key, ENTRIES_KEY])
  } else {
    key_route(&[PRESETS_KEY, model_key, ENTRIES_KEY, name])
  };

  let doc = Document::new(source.clone()).map_err(|e| WriteError::Patch(e.to_string()))?;
  // `yamlpatch` Op::Remove on a flow-style container wipes its siblings;
  // refuse rather than silently destroying hand-authored entries.
  if !is_block_mapping(&doc, &key_route(&[PRESETS_KEY, model_key, ENTRIES_KEY])) {
    return Err(flow_err(ENTRIES_KEY));
  }
  let patched = apply_yaml_patches(
    &doc,
    std::slice::from_ref(&Patch {
      route,
      operation: Op::Remove,
    }),
  )
  .map_err(|e| WriteError::Patch(e.to_string()))?;
  write(config_path, patched.source())?;
  Ok(true)
}

/// Produce the new document text for an upsert by splicing the rendered
/// body into the shallowest place that needs to change.
fn splice_upsert(
  source: &str,
  model_key: &str,
  name: &str,
  flow: &str,
) -> Result<String, WriteError> {
  // Keys are quoted when a bare token wouldn't be a plain string.
  let mk = yaml_key(model_key);
  let nk = yaml_key(name);

  if source.trim().is_empty() {
    return Ok(format!(
      "presets:\n{m}{mk}:\n{e}entries:\n{n}{nk}: {flow}\n",
      m = pad(STEP),
      e = pad(STEP * 2),
      n = pad(STEP * 3),
    ));
  }

  let current = parse(source, Path::new("config.yaml"))?;
  let presets = current.get(PRESETS_KEY);
  let model = presets.and_then(|p| p.get(model_key));
  let entries = model.and_then(|m| m.get(ENTRIES_KEY));
  let entry_exists = entries.and_then(|e| e.get(name)).is_some();

  let doc = Document::new(source.to_string()).map_err(|e| WriteError::Patch(e.to_string()))?;
  let p_route = key_route(&[PRESETS_KEY]);
  let m_route = key_route(&[PRESETS_KEY, model_key]);
  let e_route = key_route(&[PRESETS_KEY, model_key, ENTRIES_KEY]);
  let entry_route = key_route(&[PRESETS_KEY, model_key, ENTRIES_KEY, name]);

  let entries_is_nonempty = entries
    .and_then(YamlValue::as_mapping)
    .is_some_and(|m| !m.is_empty());

  // Block-style only. A hand-authored flow / scalar container on the edit
  // path would be corrupted by a block insert; refuse cleanly instead.
  // (An empty `entries: {}` is the one exception — repaired by the replace
  // path below — so it isn't guarded here.)
  if presets.is_some() && !is_block_mapping(&doc, &p_route) {
    return Err(flow_err(PRESETS_KEY));
  }
  if model.is_some() && !is_block_mapping(&doc, &m_route) {
    return Err(flow_err(model_key));
  }
  if entries_is_nonempty && !is_block_mapping(&doc, &e_route) {
    return Err(flow_err(ENTRIES_KEY));
  }

  if entry_exists {
    // Replace just the value span; key, indentation, and any trailing
    // comment on the line survive untouched.
    let span = value_span(&doc, &entry_route)?;
    Ok(replace_span(source, span, flow))
  } else if entries_is_nonempty {
    let indent = child_indent(&doc, &e_route).unwrap_or(STEP * 3);
    let at = append_point(source, &doc, &e_route, last_key(entries))?;
    Ok(insert_at(
      source,
      at,
      &format!("{}{nk}: {flow}", pad(indent)),
    ))
  } else if entries.is_some() {
    // Degenerate `entries:` (null / empty / flow) — replace the whole
    // `entries:` node with a fresh, populated block.
    let f = doc
      .query_pretty(&e_route)
      .map_err(|e| WriteError::Patch(e.to_string()))?;
    let indent = f.location.point_span.0 .1;
    let block = format!("{ENTRIES_KEY}:\n{}{nk}: {flow}", pad(indent + STEP));
    Ok(replace_span(source, f.location.byte_span, &block))
  } else if model.is_some() {
    let indent = key_column(&doc, &m_route).unwrap_or(STEP);
    let at = append_point(source, &doc, &m_route, last_key(model))?;
    let block = format!(
      "{e}entries:\n{n}{nk}: {flow}",
      e = pad(indent + STEP),
      n = pad(indent + STEP * 2),
    );
    Ok(insert_at(source, at, &block))
  } else if presets.is_some() {
    let indent = child_indent(&doc, &p_route).unwrap_or(STEP);
    let at = append_point(source, &doc, &p_route, last_key(presets))?;
    let block = format!(
      "{m}{mk}:\n{e}entries:\n{n}{nk}: {flow}",
      m = pad(indent),
      e = pad(indent + STEP),
      n = pad(indent + STEP * 2),
    );
    Ok(insert_at(source, at, &block))
  } else {
    // `presets:` key absent entirely → append the block at end of file.
    let block = format!(
      "presets:\n{m}{mk}:\n{e}entries:\n{n}{nk}: {flow}",
      m = pad(STEP),
      e = pad(STEP * 2),
      n = pad(STEP * 3),
    );
    Ok(insert_at(source, source.len(), &block))
  }
}

/// Render `key` as a YAML mapping key, quoting it when a bare token would
/// parse as a non-string scalar (pure digits, `true`/`null`/…) or carries
/// YAML-special characters. Without this, a preset named `12345` or `true`
/// would round-trip as an int/bool key, so a later update wouldn't match it
/// by string and would append a duplicate key (corrupting the file).
fn yaml_key(key: &str) -> String {
  if is_safe_plain_key(key) {
    key.to_string()
  } else {
    // JSON string syntax is valid YAML double-quoted scalar syntax.
    serde_json::to_string(key).unwrap_or_else(|_| format!("{key:?}"))
  }
}

fn is_safe_plain_key(key: &str) -> bool {
  let mut chars = key.chars();
  let first_ok = chars
    .next()
    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
  let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
  if !(first_ok && rest_ok) {
    return false;
  }
  // Reject YAML boolean / null keywords that would parse as non-strings.
  !matches!(
    key.to_ascii_lowercase().as_str(),
    "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "y" | "n"
  )
}

/// True when the node at `route` is a **block** mapping. The writer only
/// edits block style; a hand-authored flow container (`{ … }`), a scalar,
/// or a sequence at a route we'd splice into would be corrupted by a block
/// insert or a `yamlpatch` remove, so callers refuse those.
fn is_block_mapping(doc: &Document, route: &Route<'_>) -> bool {
  matches!(
    doc.query_exact(route).ok().flatten().map(|f| f.kind()),
    Some(FeatureKind::BlockMapping)
  )
}

fn flow_err(key: &str) -> WriteError {
  WriteError::Patch(format!(
    "config presets `{key}` uses flow / non-block style; edit it by hand or convert it to block style before saving presets there"
  ))
}

fn key_route(keys: &[&str]) -> Route<'static> {
  Route::from(
    keys
      .iter()
      .map(|k| Component::Key(k.to_string().into()))
      .collect::<Vec<_>>(),
  )
}

/// Byte span of the *value* at `route` (key and comments excluded).
fn value_span(doc: &Document, route: &Route<'_>) -> Result<(usize, usize), WriteError> {
  let feature = doc
    .query_exact(route)
    .map_err(|e| WriteError::Patch(e.to_string()))?
    .ok_or_else(|| WriteError::Patch(format!("no value at {route:?}")))?;
  Ok(feature.location.byte_span)
}

/// Byte offset just past the end of the block mapping rooted at `route` —
/// the point a new sibling line is inserted at.
fn block_end(doc: &Document, route: &Route<'_>) -> Result<usize, WriteError> {
  Ok(value_span(doc, route)?.1)
}

/// Insertion point for a new child of the mapping at `container`: the byte
/// just past the *last child's own line*. tree-sitter folds a trailing
/// dedented comment into the block's span, so inserting at the raw block
/// end would land a new sibling after that comment; anchoring on the last
/// child's line keeps the new entry adjacent to its siblings instead.
/// Falls back to the raw block end when the last child can't be located.
fn append_point(
  source: &str,
  doc: &Document,
  container: &Route<'_>,
  last_child: Option<String>,
) -> Result<usize, WriteError> {
  let Some(last) = last_child else {
    return block_end(doc, container);
  };
  let span = match value_span(doc, &container.with_key(last)) {
    Ok(s) => s,
    Err(_) => return block_end(doc, container),
  };
  // End of the line holding that value (skipping a trailing inline comment).
  Ok(
    source[span.1..]
      .find('\n')
      .map(|i| span.1 + i + 1)
      .unwrap_or(source.len()),
  )
}

/// Last key of the block mapping `node`, if it is a non-empty mapping.
fn last_key(node: Option<&YamlValue>) -> Option<String> {
  node
    .and_then(YamlValue::as_mapping)
    .and_then(|m| m.keys().last())
    .and_then(YamlValue::as_str)
    .map(str::to_string)
}

/// Column the first child of the mapping at `route` starts at (its
/// indentation). `None` when the route has no block-mapping value.
fn child_indent(doc: &Document, route: &Route<'_>) -> Option<usize> {
  doc
    .query_exact(route)
    .ok()
    .flatten()
    .map(|f| f.location.point_span.0 .1)
}

/// Column the key at `route` starts at.
fn key_column(doc: &Document, route: &Route<'_>) -> Option<usize> {
  doc
    .query_key_only(route)
    .ok()
    .map(|f| f.location.point_span.0 .1)
}

fn replace_span(source: &str, span: (usize, usize), value: &str) -> String {
  let mut out = source.to_string();
  out.replace_range(span.0..span.1, value);
  out
}

/// Insert `block` (one or more lines, no trailing newline) as a fresh
/// line at byte offset `at`, adding separators so the result stays
/// well-formed regardless of whether `at` already sits on a line break.
fn insert_at(source: &str, at: usize, block: &str) -> String {
  let needs_leading = at > 0 && source.as_bytes().get(at - 1) != Some(&b'\n');
  let mut ins = String::new();
  if needs_leading {
    ins.push('\n');
  }
  ins.push_str(block);
  ins.push('\n');
  let mut out = source.to_string();
  out.insert_str(at, &ins);
  out
}

fn pad(n: usize) -> String {
  " ".repeat(n)
}

fn serialise(body: &impl Serialize) -> Result<serde_json::Value, WriteError> {
  serde_json::to_value(body).map_err(|e| WriteError::Serialise(e.to_string()))
}

/// Drop top-level `null` values so an entry only carries set fields (a
/// default [`crate::config::TypedKnobs`] serialises all 19 knobs, most as
/// `null`). The body is flat, so a single-level prune suffices; the
/// `{auto: true}` Auto sentinel is an object, never `null`, and survives.
fn prune_nulls(value: serde_json::Value) -> serde_json::Value {
  match value {
    serde_json::Value::Object(map) => {
      serde_json::Value::Object(map.into_iter().filter(|(_, v)| !v.is_null()).collect())
    }
    other => other,
  }
}

fn parse(source: &str, path: &Path) -> Result<YamlValue, WriteError> {
  yaml_serde::from_str(source).map_err(|e| WriteError::ParseCurrent {
    path: path.to_path_buf(),
    error: e.to_string(),
  })
}

fn read_source(path: &Path) -> Result<String, WriteError> {
  match std::fs::read_to_string(path) {
    Ok(s) => Ok(s),
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
    Err(e) => Err(WriteError::Io {
      path: path.to_path_buf(),
      error: e.to_string(),
    }),
  }
}

fn write(path: &Path, body: &str) -> Result<(), WriteError> {
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent).map_err(|e| WriteError::Io {
      path: parent.to_path_buf(),
      error: e.to_string(),
    })?;
  }
  let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
  crate::util::atomic_write::write_secure(
    &dir,
    "config.yaml.tmp.",
    path,
    body.as_bytes(),
    Some(0o600),
  )
  .map_err(|e| WriteError::Io {
    path: path.to_path_buf(),
    error: e.to_string(),
  })?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::BTreeMap;
  use std::fs;
  use std::path::PathBuf;

  fn temp_dir(label: &str) -> PathBuf {
    crate::util::test_temp::unique_temp_dir(&format!("presets-writer-{label}"))
  }

  /// A minimal flat preset body for tests — mirrors the real
  /// `PresetBody` shape (scalars + a nested `{auto: true}` + an extras
  /// list) without depending on its definition.
  fn body(pairs: &[(&str, serde_json::Value)]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (k, v) in pairs {
      map.insert((*k).to_string(), v.clone());
    }
    serde_json::Value::Object(map)
  }

  fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
  }

  fn entries_of<'a>(yaml: &'a YamlValue, model: &str) -> &'a yaml_serde::Mapping {
    yaml
      .get("presets")
      .and_then(|p| p.get(model))
      .and_then(|m| m.get("entries"))
      .and_then(YamlValue::as_mapping)
      .expect("entries map")
  }

  fn ctx_of(yaml: &YamlValue, model: &str, name: &str) -> Option<u64> {
    entries_of(yaml, model)
      .get(name)
      .and_then(|e| e.get("ctx"))
      .and_then(YamlValue::as_u64)
  }

  #[test]
  fn create_under_new_model_key_in_empty_file() {
    let dir = temp_dir("empty");
    let path = dir.join("config.yaml");
    upsert_preset(
      &path,
      "qwen-coder",
      "long-ctx",
      &body(&[("ctx", 65536.into())]),
    )
    .unwrap();
    let yaml: YamlValue = yaml_serde::from_str(&read(&path)).unwrap();
    assert_eq!(ctx_of(&yaml, "qwen-coder", "long-ctx"), Some(65536));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn create_appends_under_existing_presets_block() {
    let dir = temp_dir("append-model");
    let path = dir.join("config.yaml");
    fs::write(
      &path,
      "theme: latte\npresets:\n  other-model:\n    entries:\n      fast: { ctx: 4096 }\n",
    )
    .unwrap();
    upsert_preset(
      &path,
      "qwen-coder",
      "long-ctx",
      &body(&[("ctx", 65536.into())]),
    )
    .unwrap();
    let text = read(&path);
    assert!(text.contains("theme: latte"), "unrelated key survives");
    let yaml: YamlValue = yaml_serde::from_str(&text).unwrap();
    assert_eq!(
      ctx_of(&yaml, "other-model", "fast"),
      Some(4096),
      "sibling model untouched"
    );
    assert_eq!(ctx_of(&yaml, "qwen-coder", "long-ctx"), Some(65536));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn update_existing_entry_preserves_all_comments() {
    let dir = temp_dir("update-comments");
    let path = dir.join("config.yaml");
    let original = "\
# top of file comment
theme: latte  # inline kept

presets:
  qwen-coder:
    entries:
      short-ctx: { ctx: 8192 }   # the small one
      long-ctx: { ctx: 32768 }   # bump this
";
    fs::write(&path, original).unwrap();
    upsert_preset(
      &path,
      "qwen-coder",
      "long-ctx",
      &body(&[("ctx", 65536.into())]),
    )
    .unwrap();
    let text = read(&path);
    assert!(text.contains("# top of file comment"));
    assert!(text.contains("theme: latte  # inline kept"));
    assert!(text.contains("short-ctx: { ctx: 8192 }   # the small one"));
    assert!(
      text.contains("# bump this"),
      "the edited line's trailing comment survives"
    );
    let yaml: YamlValue = yaml_serde::from_str(&text).unwrap();
    assert_eq!(
      ctx_of(&yaml, "qwen-coder", "long-ctx"),
      Some(65536),
      "value actually changed"
    );
    assert_eq!(
      ctx_of(&yaml, "qwen-coder", "short-ctx"),
      Some(8192),
      "sibling unchanged"
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn app_write_to_one_model_leaves_hand_authored_arch_preset_untouched() {
    let dir = temp_dir("arch-untouched");
    let path = dir.join("config.yaml");
    let original = "\
presets:
  # hand-authored arch-level preset — must never be rewritten by an app save
  qwen2:
    default: balanced
    entries:
      balanced: { ctx: 16384, flash_attn: true }  # tuned by hand
";
    fs::write(&path, original).unwrap();
    upsert_preset(
      &path,
      "some-model.gguf",
      "coding",
      &body(&[("ctx", 8192.into())]),
    )
    .unwrap();
    let text = read(&path);
    assert!(text.contains("# hand-authored arch-level preset"));
    assert!(text.contains("default: balanced"));
    assert!(text.contains("balanced: { ctx: 16384, flash_attn: true }  # tuned by hand"));
    let yaml: YamlValue = yaml_serde::from_str(&text).unwrap();
    assert_eq!(ctx_of(&yaml, "some-model.gguf", "coding"), Some(8192));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn round_trips_scalar_bool_auto_and_numeric_string_extras() {
    let dir = temp_dir("round-trip");
    let path = dir.join("config.yaml");
    let b = body(&[
      ("ctx", 32768.into()),
      ("flash_attn", true.into()),
      ("n_gpu_layers", serde_json::json!({ "auto": true })),
      ("extras", serde_json::json!(["--rope-freq-base", "10000"])),
      ("dropped", serde_json::Value::Null),
    ]);
    upsert_preset(&path, "m", "p", &b).unwrap();
    let yaml: YamlValue = yaml_serde::from_str(&read(&path)).unwrap();
    let entry = entries_of(&yaml, "m").get("p").unwrap();
    assert_eq!(entry.get("ctx").unwrap().as_u64(), Some(32768));
    assert_eq!(entry.get("flash_attn").unwrap().as_bool(), Some(true));
    assert!(
      entry.get("n_gpu_layers").unwrap().get("auto").is_some(),
      "Auto sentinel survives"
    );
    let extras = entry.get("extras").unwrap().as_sequence().unwrap();
    assert_eq!(extras.len(), 2, "extras list round-trips");
    assert_eq!(
      extras[1].as_str(),
      Some("10000"),
      "a numeric-looking extras token stays a string, not an int"
    );
    assert!(entry.get("dropped").is_none(), "null fields are pruned out");
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn delete_entry_keeps_siblings_and_their_comments() {
    let dir = temp_dir("delete-entry");
    let path = dir.join("config.yaml");
    let original = "\
presets:
  qwen-coder:
    entries:
      short-ctx: { ctx: 8192 }   # keep me
      long-ctx: { ctx: 32768 }
";
    fs::write(&path, original).unwrap();
    assert!(remove_preset(&path, "qwen-coder", "long-ctx").unwrap());
    let text = read(&path);
    assert!(text.contains("short-ctx: { ctx: 8192 }   # keep me"));
    let yaml: YamlValue = yaml_serde::from_str(&text).unwrap();
    let entries = entries_of(&yaml, "qwen-coder");
    assert!(entries.contains_key("short-ctx"));
    assert!(!entries.contains_key("long-ctx"));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn delete_json_flow_entry_written_by_upsert() {
    // A round-trip: upsert (writes compact JSON flow) then delete must
    // find and remove that entry via the yamlpatch Remove path.
    let dir = temp_dir("delete-json");
    let path = dir.join("config.yaml");
    upsert_preset(&path, "m", "a", &body(&[("ctx", 1.into())])).unwrap();
    upsert_preset(&path, "m", "b", &body(&[("ctx", 2.into())])).unwrap();
    assert!(remove_preset(&path, "m", "a").unwrap());
    let yaml: YamlValue = yaml_serde::from_str(&read(&path)).unwrap();
    let entries = entries_of(&yaml, "m");
    assert!(!entries.contains_key("a"));
    assert!(entries.contains_key("b"));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn delete_last_entry_prunes_model_key() {
    let dir = temp_dir("delete-prune");
    let path = dir.join("config.yaml");
    fs::write(
      &path,
      "theme: latte\npresets:\n  m:\n    entries:\n      only: { ctx: 8192 }\n",
    )
    .unwrap();
    assert!(remove_preset(&path, "m", "only").unwrap());
    let yaml: YamlValue = yaml_serde::from_str(&read(&path)).unwrap();
    assert!(
      yaml.get("presets").and_then(|p| p.get("m")).is_none(),
      "now-empty model key is pruned"
    );
    assert!(
      read(&path).contains("theme: latte"),
      "unrelated key survives the prune"
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn delete_last_entry_keeps_model_key_when_default_present() {
    let dir = temp_dir("delete-keep-default");
    let path = dir.join("config.yaml");
    fs::write(
      &path,
      "presets:\n  m:\n    default: only\n    entries:\n      only: { ctx: 8192 }\n",
    )
    .unwrap();
    assert!(remove_preset(&path, "m", "only").unwrap());
    let text = read(&path);
    assert!(
      text.contains("default: only"),
      "hand-authored default survives"
    );
    let yaml: YamlValue = yaml_serde::from_str(&text).unwrap();
    assert!(
      yaml
        .get("presets")
        .and_then(|p| p.get("m"))
        .and_then(|m| m.get("entries"))
        .is_none(),
      "only the empty entries map is pruned"
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn delete_absent_preset_is_a_noop_returning_false() {
    let dir = temp_dir("delete-absent");
    let path = dir.join("config.yaml");
    fs::write(
      &path,
      "presets:\n  m:\n    entries:\n      only: { ctx: 8192 }\n",
    )
    .unwrap();
    let before = read(&path);
    assert!(!remove_preset(&path, "m", "nope").unwrap());
    assert!(!remove_preset(&path, "other", "only").unwrap());
    assert_eq!(read(&path), before, "no write on a no-op delete");
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn create_into_block_that_has_default_but_no_entries() {
    let dir = temp_dir("default-no-entries");
    let path = dir.join("config.yaml");
    fs::write(&path, "presets:\n  m:\n    default: coding\n").unwrap();
    upsert_preset(&path, "m", "coding", &body(&[("ctx", 8192.into())])).unwrap();
    let text = read(&path);
    assert!(text.contains("default: coding"));
    let yaml: YamlValue = yaml_serde::from_str(&text).unwrap();
    assert_eq!(ctx_of(&yaml, "m", "coding"), Some(8192));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn create_when_entries_is_empty_flow_map() {
    // Degenerate hand-authored `entries: {}` — replaced with a populated
    // block; the model key and any siblings survive.
    let dir = temp_dir("empty-entries");
    let path = dir.join("config.yaml");
    fs::write(&path, "presets:\n  m:\n    entries: {}\n").unwrap();
    upsert_preset(&path, "m", "coding", &body(&[("ctx", 8192.into())])).unwrap();
    let yaml: YamlValue = yaml_serde::from_str(&read(&path)).unwrap();
    assert_eq!(ctx_of(&yaml, "m", "coding"), Some(8192));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn upsert_is_atomic_no_tmp_lingers() {
    let dir = temp_dir("atomic");
    let path = dir.join("config.yaml");
    upsert_preset(&path, "m", "p", &body(&[("ctx", 8192.into())])).unwrap();
    for entry in fs::read_dir(&dir).unwrap() {
      let name = entry.unwrap().file_name();
      assert!(
        !name.to_string_lossy().starts_with("config.yaml.tmp"),
        ".tmp sibling lingered"
      );
    }
    fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn upsert_sets_mode_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir("mode");
    let path = dir.join("config.yaml");
    upsert_preset(&path, "m", "p", &body(&[("ctx", 8192.into())])).unwrap();
    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn refuses_symlink_target() {
    use std::os::unix::fs::symlink;
    let dir = temp_dir("symlink");
    let victim = dir.join("victim.dat");
    fs::write(&victim, b"important").unwrap();
    let path = dir.join("config.yaml");
    symlink(&victim, &path).unwrap();
    let err = upsert_preset(&path, "m", "p", &body(&[("ctx", 8192.into())])).unwrap_err();
    assert!(matches!(err, WriteError::TargetIsSymlink { .. }));
    assert_eq!(fs::read(&victim).unwrap(), b"important", "victim untouched");
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn three_writes_across_two_models_keep_every_entry() {
    let dir = temp_dir("two-models");
    let path = dir.join("config.yaml");
    upsert_preset(&path, "a", "p1", &body(&[("ctx", 1024.into())])).unwrap();
    upsert_preset(&path, "b", "p2", &body(&[("ctx", 2048.into())])).unwrap();
    upsert_preset(&path, "a", "p3", &body(&[("ctx", 4096.into())])).unwrap();
    let yaml: YamlValue = yaml_serde::from_str(&read(&path)).unwrap();
    let a: BTreeMap<String, YamlValue> = yaml_serde::from_value(
      yaml
        .get("presets")
        .unwrap()
        .get("a")
        .unwrap()
        .get("entries")
        .unwrap()
        .clone(),
    )
    .unwrap();
    assert_eq!(a.len(), 2, "model a has both p1 and p3");
    assert_eq!(ctx_of(&yaml, "a", "p1"), Some(1024));
    assert_eq!(ctx_of(&yaml, "a", "p3"), Some(4096));
    assert_eq!(ctx_of(&yaml, "b", "p2"), Some(2048));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn numeric_and_keyword_preset_names_are_quoted_and_update_in_place() {
    // A name that bare-parses as a non-string scalar (digits, `true`)
    // must be quoted so a second save UPDATES it rather than appending a
    // duplicate key (which would make the file unparseable).
    let dir = temp_dir("tautology");
    let path = dir.join("config.yaml");
    for name in ["12345", "true", "null"] {
      upsert_preset(&path, "m", name, &body(&[("ctx", 1.into())])).unwrap();
      upsert_preset(&path, "m", name, &body(&[("ctx", 2.into())])).unwrap();
      // Re-reads cleanly (no duplicate key) and reflects the update.
      let yaml: YamlValue = yaml_serde::from_str(&read(&path)).unwrap();
      let entries = entries_of(&yaml, "m");
      let got = entries
        .get(name)
        .unwrap_or_else(|| panic!("{name} present"));
      assert_eq!(
        got.get("ctx").and_then(YamlValue::as_u64),
        Some(2),
        "{name} updated in place"
      );
      assert!(
        read(&path).contains(&format!("\"{name}\":")),
        "{name} written quoted"
      );
      remove_preset(&path, "m", name).unwrap();
    }
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn refuses_to_add_into_a_flow_style_entries_block() {
    // A hand-authored flow container must not be block-spliced (that
    // produces invalid YAML). Refuse cleanly; leave the file untouched.
    let dir = temp_dir("flow-add");
    let path = dir.join("config.yaml");
    let original = "presets:\n  qwen2: { entries: { balanced: { ctx: 16384 } } }\n";
    fs::write(&path, original).unwrap();
    let err = upsert_preset(&path, "qwen2", "fast", &body(&[("ctx", 4096.into())])).unwrap_err();
    assert!(matches!(err, WriteError::Patch(_)));
    assert_eq!(read(&path), original, "file untouched on refusal");
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn refuses_to_delete_from_a_flow_style_entries_block() {
    // Op::Remove on a flow mapping wipes siblings — refuse instead.
    let dir = temp_dir("flow-del");
    let path = dir.join("config.yaml");
    let original =
      "presets:\n  qwen2: { entries: { balanced: { ctx: 16384 }, fast: { ctx: 4096 } } }\n";
    fs::write(&path, original).unwrap();
    let err = remove_preset(&path, "qwen2", "fast").unwrap_err();
    assert!(matches!(err, WriteError::Patch(_)));
    assert_eq!(read(&path), original, "siblings untouched on refusal");
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn refuses_to_add_entries_under_a_scalar_model_value() {
    // A model key whose value is a scalar can't carry a block `entries:`.
    let dir = temp_dir("scalar-model");
    let path = dir.join("config.yaml");
    let original = "presets:\n  m: somescalar\n";
    fs::write(&path, original).unwrap();
    let err = upsert_preset(&path, "m", "p", &body(&[("ctx", 1.into())])).unwrap_err();
    assert!(matches!(err, WriteError::Patch(_)));
    assert_eq!(read(&path), original, "file untouched on refusal");
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn append_entry_lands_before_a_trailing_dedented_comment() {
    // A comment at a shallower indent trails the entries block. The new
    // entry must land adjacent to its sibling, not after the comment
    // (which tree-sitter folds into the block's span).
    let dir = temp_dir("trailing-comment");
    let path = dir.join("config.yaml");
    fs::write(
      &path,
      "presets:\n  m:\n    entries:\n      a: { ctx: 1 }\n  # arch presets below\n  qwen2:\n    entries:\n      b: { ctx: 2 }\n",
    )
    .unwrap();
    upsert_preset(&path, "m", "c", &body(&[("ctx", 3.into())])).unwrap();
    let text = read(&path);
    // `c` sits next to `a`, above the comment.
    let a_line = text.find("a: { ctx: 1 }").unwrap();
    let c_line = text.find("c:").unwrap();
    let comment = text.find("# arch presets below").unwrap();
    assert!(
      a_line < c_line && c_line < comment,
      "new entry stays with its siblings:\n{text}"
    );
    let yaml: YamlValue = yaml_serde::from_str(&text).unwrap();
    assert_eq!(ctx_of(&yaml, "m", "c"), Some(3));
    assert_eq!(ctx_of(&yaml, "qwen2", "b"), Some(2), "arch block intact");
    assert!(text.contains("# arch presets below"));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn append_model_before_trailing_top_level_key() {
    // presets is not the last block in the file — a new model must land
    // inside presets, not after the trailing key.
    let dir = temp_dir("trailing-key");
    let path = dir.join("config.yaml");
    fs::write(
      &path,
      "presets:\n  a:\n    entries:\n      p: { ctx: 1 }\ntheme: latte\n",
    )
    .unwrap();
    upsert_preset(&path, "b", "q", &body(&[("ctx", 2.into())])).unwrap();
    let text = read(&path);
    assert!(text.contains("theme: latte"));
    let yaml: YamlValue = yaml_serde::from_str(&text).unwrap();
    assert_eq!(ctx_of(&yaml, "a", "p"), Some(1));
    assert_eq!(ctx_of(&yaml, "b", "q"), Some(2));
    assert_eq!(yaml.get("theme").and_then(YamlValue::as_str), Some("latte"));
    fs::remove_dir_all(&dir).ok();
  }
}
