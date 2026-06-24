//! The single comment-preserving mutation primitive for `config.yaml`.
//!
//! Every write to `config.yaml` in the binary goes through here: the
//! `presets:` writer ([`crate::config::presets_writer`]) and the init / cli
//! merge writer ([`crate::config::writer::merge_and_write`]) both call
//! `upsert` / `remove`. There is exactly one splice implementation, so a
//! hand-written comment survives a save no matter which surface wrote it.
//!
//! [`yamlpath`] locates the precise byte span of any node (it tracks
//! comments and formatting via tree-sitter); we splice the rendered value
//! into that span ourselves. We do *not* use `yamlpatch`'s value-add /
//! value-replace ops for writes — they re-indent nested map / sequence
//! values incorrectly. Deletes do go through `yamlpatch`'s `Op::Remove`,
//! which drops a whole key cleanly.
//!
//! The mutation functions are pure `&str -> String` transforms so a caller
//! can fold several edits into one in-memory document before a single
//! atomic write. File I/O lives in `read_source` / `write_config`, which
//! both writers also share.

use std::path::Path;

use yaml_serde::Value as YamlValue;
use yamlpatch::{apply_yaml_patches, Op, Patch};
use yamlpath::{Component, Document, FeatureKind, Route};

use crate::config::writer::WriteError;

/// One indentation step. Matches the project's 2-space YAML convention;
/// only used when *creating* a nesting level (existing levels keep their
/// own indentation, derived from the document).
const STEP: usize = 2;

/// Create or replace the node at `path` (the full key path from the document
/// root, e.g. `["presets", "qwen2", "entries", "long-ctx"]` or
/// `["proxy", "api_key"]`) so its value becomes the rendered inline YAML
/// token `value_flow`, preserving every unrelated comment and bit of
/// formatting. Missing parent blocks along `path` are created, and a *vacant*
/// node on the path (an explicit `key:` null or an empty `{}`) is built into.
/// Refuses (with a clear error, leaving the source untouched) when a container
/// on the path holds real data a block insert would corrupt — a non-empty flow
/// mapping, a scalar, or a sequence.
pub(crate) fn upsert(source: &str, path: &[&str], value_flow: &str) -> Result<String, WriteError> {
  debug_assert!(!path.is_empty(), "upsert needs a non-empty key path");
  if source.trim().is_empty() {
    // Fresh document: render the whole chain from the root.
    return Ok(format!("{}\n", render_chain(path, value_flow, 0)));
  }

  let doc = Document::new(source.to_string()).map_err(|e| WriteError::Patch(e.to_string()))?;
  let root = parse(source, Path::new("config.yaml"))?;

  // Deepest existing prefix: `i` leading segments of `path` that resolve.
  let mut i = 0usize;
  {
    let mut cur = &root;
    while i < path.len() {
      match cur.get(path[i]) {
        Some(child) => {
          cur = child;
          i += 1;
        }
        None => break,
      }
    }
  }

  // Every container strictly above the deepest existing node must be a block
  // mapping — `yaml_serde` happily navigates *into* a flow container, so we'd
  // otherwise splice block lines into `{ ... }` and corrupt it.
  guard_ancestors_block(&doc, path, i)?;

  if i == path.len() {
    // Full path exists → replace just the leaf's value span (key,
    // indentation, and any trailing comment on the line survive).
    let span = value_span(&doc, &key_route(path))?;
    return Ok(replace_span(source, span, value_flow));
  }

  if i > 0 {
    // The deepest existing node `path[0..i]` is where the missing suffix
    // attaches. Decide by its shape.
    let node_i = resolve(&root, &path[..i]).expect("prefix resolves");
    if node_i.as_mapping().is_some_and(|m| !m.is_empty()) {
      // Non-empty mapping: a block append only works on block style; a
      // hand-authored flow `{ ... }` would be mangled, so refuse it.
      if !is_block_mapping(&doc, &key_route(&path[..i])) {
        return Err(flow_err(path[i - 1]));
      }
      // else: non-empty block — fall through to the append below.
    } else if is_vacant(node_i) {
      // The deepest existing node is *vacant* — `key:` null or an empty `{}` —
      // so nothing is lost by building into it. Re-emit the key with a fresh
      // block holding the whole missing suffix (one level deep or several).
      let f = doc
        .query_pretty(&key_route(&path[..i]))
        .map_err(|e| WriteError::Patch(e.to_string()))?;
      let keycol = f.location.point_span.0 .1;
      let block = render_keyed_chain(path[i - 1], &path[i..], value_flow, keycol);
      return Ok(replace_span(source, f.location.byte_span, &block));
    } else {
      // The node holds a scalar or a sequence — real data a nested write would
      // clobber. Refuse and leave the file untouched.
      return Err(scalar_err(path[i - 1]));
    }
  }

  // Append the missing suffix `path[i..]` as a new child of the container at
  // `path[0..i]` (the document root when `i == 0`).
  let container = key_route(&path[..i]);
  let indent = if i == 0 {
    0
  } else {
    child_indent(&doc, &container).unwrap_or(i * STEP)
  };
  let last = last_key(resolve(&root, &path[..i]));
  let at = append_point(source, &doc, &container, last)?;
  let block = render_chain(&path[i..], value_flow, indent);
  Ok(insert_at(source, at, &block))
}

/// Remove the node at `path`, pruning now-empty parent blocks up the chain
/// (deleting the last entry of a block also deletes the block, and so on).
/// Returns `Ok(None)` when the node was absent (nothing to write). Refuses
/// when the parent container is flow style (an `Op::Remove` there would wipe
/// its siblings).
pub(crate) fn remove(source: &str, path: &[&str]) -> Result<Option<String>, WriteError> {
  debug_assert!(!path.is_empty(), "remove needs a non-empty key path");
  if source.trim().is_empty() {
    return Ok(None);
  }
  let root = parse(source, Path::new("config.yaml"))?;
  if resolve(&root, path).is_none() {
    return Ok(None);
  }

  // Walk up while each parent holds *only* the child on our path: removing
  // the child would empty the parent, so prune the parent instead.
  let mut keep = path.len();
  for l in (1..path.len()).rev() {
    let only_child = resolve(&root, &path[..l])
      .and_then(YamlValue::as_mapping)
      .is_some_and(|m| m.len() == 1);
    if only_child {
      keep = l;
    } else {
      break;
    }
  }

  let doc = Document::new(source.to_string()).map_err(|e| WriteError::Patch(e.to_string()))?;
  // The container we remove *from* must be a block mapping; `Op::Remove` on a
  // flow mapping wipes its siblings.
  let parent = &path[..keep - 1];
  if !parent.is_empty() && !is_block_mapping(&doc, &key_route(parent)) {
    return Err(flow_err(parent[parent.len() - 1]));
  }

  let patched = apply_yaml_patches(
    &doc,
    std::slice::from_ref(&Patch {
      route: key_route(&path[..keep]),
      operation: Op::Remove,
    }),
  )
  .map_err(|e| WriteError::Patch(e.to_string()))?;
  Ok(Some(patched.source().to_string()))
}

/// Render a [`YamlValue`] leaf as a single-line inline YAML token suitable as
/// the `value_flow` argument to [`upsert`]. Scalars render bare where YAML
/// allows (a numeric-looking string stays quoted); a sequence / mapping
/// renders as compact JSON, which is valid single-line YAML flow.
pub(crate) fn render_value(value: &YamlValue) -> Result<String, WriteError> {
  match value {
    YamlValue::Sequence(_) | YamlValue::Mapping(_) => render_flow_json(value),
    _ => render_scalar(value),
  }
}

/// Render any serializable value as a compact single-line flow token. JSON is
/// valid single-line YAML flow with faithful typing/quoting (a numeric-looking
/// string like `"10000"` stays quoted), so this is the single place the
/// "compact JSON ≡ flow YAML" encoding lives — shared by [`render_value`] (for
/// nested seq / map leaves) and the presets writer, which builds its body as a
/// `serde_json::Value` (its sorted-key map gives stable on-disk output).
pub(crate) fn render_flow_json(value: &impl serde::Serialize) -> Result<String, WriteError> {
  serde_json::to_string(value).map_err(|e| WriteError::Serialise(e.to_string()))
}

/// Render a scalar [`YamlValue`] as a single-line inline YAML token (quoting
/// handled by the YAML serializer, e.g. a numeric-looking string stays
/// quoted).
fn render_scalar(value: &YamlValue) -> Result<String, WriteError> {
  let rendered = yaml_serde::to_string(value).map_err(|e| WriteError::Serialise(e.to_string()))?;
  let token = rendered.trim_end_matches('\n');
  if token.contains('\n') {
    // A multi-line (block) rendering can't splice as a one-line value.
    return Err(WriteError::Serialise(format!(
      "value is not a single-line scalar: {token:?}"
    )));
  }
  Ok(token.to_string())
}

// --- text rendering -------------------------------------------------------

/// Render `segs` as a nested block, the first key at column `indent`, the
/// last carrying `value_flow`. Keys are quoted when a bare token wouldn't be
/// a plain string.
fn render_chain(segs: &[&str], value_flow: &str, indent: usize) -> String {
  let mut out = String::new();
  for (d, seg) in segs.iter().enumerate() {
    let col = indent + d * STEP;
    if d + 1 == segs.len() {
      out.push_str(&format!("{}{}: {value_flow}", pad(col), yaml_key(seg)));
    } else {
      out.push_str(&format!("{}{}:\n", pad(col), yaml_key(seg)));
    }
  }
  out
}

/// Render a fresh `container_key:` re-emission followed by the `rest` chain
/// indented one step under it — used to replace a degenerate node's value.
fn render_keyed_chain(
  container_key: &str,
  rest: &[&str],
  value_flow: &str,
  keycol: usize,
) -> String {
  let mut out = format!("{}:\n", yaml_key(container_key));
  out.push_str(&render_chain(rest, value_flow, keycol + STEP));
  out
}

/// Render `key` as a YAML mapping key, quoting it when a bare token would
/// parse as a non-string scalar (pure digits, `true`/`null`/…) or carries
/// YAML-special characters. Without this, a key named `12345` or `true`
/// would round-trip as an int / bool, so a later update wouldn't match it by
/// string and would append a duplicate key (corrupting the file).
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

fn pad(n: usize) -> String {
  " ".repeat(n)
}

// --- yamlpath helpers -----------------------------------------------------

fn resolve<'a>(root: &'a YamlValue, prefix: &[&str]) -> Option<&'a YamlValue> {
  let mut cur = root;
  for k in prefix {
    cur = cur.get(k)?;
  }
  Some(cur)
}

fn key_route(keys: &[&str]) -> Route<'static> {
  Route::from(
    keys
      .iter()
      .map(|k| Component::Key(k.to_string().into()))
      .collect::<Vec<_>>(),
  )
}

/// True when the node at `route` is a **block** mapping.
fn is_block_mapping(doc: &Document, route: &Route<'_>) -> bool {
  matches!(
    doc.query_exact(route).ok().flatten().map(|f| f.kind()),
    Some(FeatureKind::BlockMapping)
  )
}

/// Require every existing container strictly above depth `i` to be a block
/// mapping (so we never splice into a flow / scalar ancestor).
fn guard_ancestors_block(doc: &Document, path: &[&str], i: usize) -> Result<(), WriteError> {
  for l in 1..i {
    if !is_block_mapping(doc, &key_route(&path[..l])) {
      return Err(flow_err(path[l - 1]));
    }
  }
  Ok(())
}

fn flow_err(key: &str) -> WriteError {
  WriteError::Patch(format!(
    "config key `{key}` uses flow / non-block style; edit it by hand or convert it to block style before writing there"
  ))
}

fn scalar_err(key: &str) -> WriteError {
  WriteError::Patch(format!(
    "config key `{key}` holds a scalar / sequence value, not a block mapping; can't create nested keys under it — clear it or make it a mapping, or edit by hand"
  ))
}

/// A node we can safely build a nested block into: an explicit null (a bare
/// `key:`) or an empty mapping (`{}`). A scalar or sequence is *not* vacant —
/// it holds data a nested write would clobber, so the caller refuses instead.
fn is_vacant(node: &YamlValue) -> bool {
  matches!(node, YamlValue::Null) || node.as_mapping().is_some_and(|m| m.is_empty())
}

/// Byte span of the *value* at `route` (key and comments excluded).
fn value_span(doc: &Document, route: &Route<'_>) -> Result<(usize, usize), WriteError> {
  let feature = doc
    .query_exact(route)
    .map_err(|e| WriteError::Patch(e.to_string()))?
    .ok_or_else(|| WriteError::Patch(format!("no value at {route:?}")))?;
  Ok(feature.location.byte_span)
}

/// Byte offset just past the end of the block mapping rooted at `route`.
fn block_end(doc: &Document, route: &Route<'_>) -> Result<usize, WriteError> {
  Ok(value_span(doc, route)?.1)
}

/// Insertion point for a new child of the mapping at `container`: the byte
/// just past the *last child's own line*. tree-sitter folds a trailing
/// dedented comment into the block's span, so inserting at the raw block end
/// would land a new sibling after that comment; anchoring on the last child's
/// line keeps the new entry adjacent to its siblings instead. Falls back to
/// the raw block end (or EOF for the root) when the last child can't be
/// located.
fn append_point(
  source: &str,
  doc: &Document,
  container: &Route<'_>,
  last_child: Option<String>,
) -> Result<usize, WriteError> {
  let Some(last) = last_child else {
    // Root container has no addressable route; fall back to EOF.
    return block_end(doc, container).or(Ok(source.len()));
  };
  let span = match value_span(doc, &container.with_key(last)) {
    Ok(s) => s,
    Err(_) => return block_end(doc, container).or(Ok(source.len())),
  };
  // End of the line holding that value (skipping a trailing inline comment).
  Ok(
    source[span.1..]
      .find('\n')
      .map(|n| span.1 + n + 1)
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

/// Column the first child of the mapping at `route` starts at.
fn child_indent(doc: &Document, route: &Route<'_>) -> Option<usize> {
  doc
    .query_exact(route)
    .ok()
    .flatten()
    .map(|f| f.location.point_span.0 .1)
}

fn replace_span(source: &str, span: (usize, usize), value: &str) -> String {
  let mut out = source.to_string();
  out.replace_range(span.0..span.1, value);
  out
}

/// Insert `block` (one or more lines, no trailing newline) as a fresh line at
/// byte offset `at`, adding separators so the result stays well-formed
/// regardless of whether `at` already sits on a line break.
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

// --- file I/O (shared by both writers) ------------------------------------

fn parse(source: &str, path: &Path) -> Result<YamlValue, WriteError> {
  yaml_serde::from_str(source).map_err(|e| WriteError::ParseCurrent {
    path: path.to_path_buf(),
    error: e.to_string(),
  })
}

/// Read `config.yaml`, treating a missing file as empty.
pub(crate) fn read_source(path: &Path) -> Result<String, WriteError> {
  match std::fs::read_to_string(path) {
    Ok(s) => Ok(s),
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
    Err(e) => Err(WriteError::Io {
      path: path.to_path_buf(),
      error: e.to_string(),
    }),
  }
}

/// Atomically write `body` to `path` (mode 0600), via the shared
/// [`crate::util::atomic_write::write_secure`] guards.
pub(crate) fn write_config(path: &Path, body: &str) -> Result<(), WriteError> {
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

  fn parse_ok(s: &str) -> YamlValue {
    yaml_serde::from_str(s).expect("valid yaml")
  }

  #[test]
  fn upsert_into_empty_source_writes_fresh() {
    let out = upsert("", &["llama_server_path"], "/opt/llama-server").unwrap();
    assert_eq!(out, "llama_server_path: /opt/llama-server\n");
  }

  #[test]
  fn upsert_top_level_scalar_preserves_comments() {
    let src = "# my config\ntheme: latte  # keep me\n";
    let out = upsert(src, &["llama_server_path"], "/opt/ls").unwrap();
    assert!(out.contains("# my config"), "header comment survives");
    assert!(
      out.contains("theme: latte  # keep me"),
      "inline comment survives"
    );
    let y = parse_ok(&out);
    assert_eq!(
      y.get("llama_server_path").and_then(YamlValue::as_str),
      Some("/opt/ls")
    );
    assert_eq!(y.get("theme").and_then(YamlValue::as_str), Some("latte"));
  }

  #[test]
  fn upsert_nested_creates_missing_parent_block() {
    let out = upsert("theme: latte\n", &["proxy", "api_key"], "sekret").unwrap();
    assert!(out.contains("theme: latte"), "unrelated key survives");
    let y = parse_ok(&out);
    assert_eq!(
      y.get("proxy")
        .and_then(|p| p.get("api_key"))
        .and_then(YamlValue::as_str),
      Some("sekret")
    );
  }

  #[test]
  fn upsert_nested_appends_into_existing_block_keeping_comment() {
    let src = "proxy:\n  port: 11500  # pinned\n";
    let out = upsert(src, &["proxy", "api_key"], "sekret").unwrap();
    assert!(
      out.contains("port: 11500  # pinned"),
      "sibling + comment kept"
    );
    let y = parse_ok(&out);
    assert_eq!(
      y.get("proxy")
        .and_then(|p| p.get("api_key"))
        .and_then(YamlValue::as_str),
      Some("sekret")
    );
    assert_eq!(
      y.get("proxy")
        .and_then(|p| p.get("port"))
        .and_then(YamlValue::as_u64),
      Some(11500)
    );
  }

  #[test]
  fn upsert_replaces_existing_leaf_keeping_comment() {
    let src = "proxy:\n  api_key: old  # secret\n";
    let out = upsert(src, &["proxy", "api_key"], "new").unwrap();
    assert!(
      out.contains("# secret"),
      "trailing comment survives a replace"
    );
    let y = parse_ok(&out);
    assert_eq!(
      y.get("proxy")
        .and_then(|p| p.get("api_key"))
        .and_then(YamlValue::as_str),
      Some("new")
    );
  }

  #[test]
  fn upsert_into_null_valued_parent_reemits_block() {
    // `proxy:` has a null value (a bare key) — adding `proxy.api_key` must
    // re-emit it as a block, not error, and keep the sibling + its comment.
    let src = "proxy:\ntheme: latte  # mine\n";
    let out = upsert(src, &["proxy", "api_key"], "sekret").unwrap();
    assert!(
      out.contains("theme: latte  # mine"),
      "sibling + comment kept"
    );
    let y = parse_ok(&out);
    assert_eq!(
      y.get("proxy")
        .and_then(|p| p.get("api_key"))
        .and_then(YamlValue::as_str),
      Some("sekret")
    );
  }

  #[test]
  fn upsert_into_null_intermediate_builds_deep_chain() {
    // `presets:` is a bare (null) key; writing three levels under it used to
    // refuse with a misleading "flow / non-block" error. It now re-emits the
    // whole missing suffix as a nested block.
    let src = "presets:\ntheme: latte\n";
    let out = upsert(src, &["presets", "m", "entries", "p"], "{\"ctx\":8192}").unwrap();
    assert!(out.contains("theme: latte"), "sibling survives");
    let y = parse_ok(&out);
    assert_eq!(
      y.get("presets")
        .and_then(|p| p.get("m"))
        .and_then(|m| m.get("entries"))
        .and_then(|e| e.get("p"))
        .and_then(|p| p.get("ctx"))
        .and_then(YamlValue::as_u64),
      Some(8192)
    );
  }

  #[test]
  fn upsert_into_empty_flow_mapping_reemits_block() {
    // `proxy: {}` (empty flow) is degenerate — adding a key re-emits it as a
    // block rather than mangling the flow braces.
    let out = upsert("proxy: {}\n", &["proxy", "api_key"], "sekret").unwrap();
    let y = parse_ok(&out);
    assert_eq!(
      y.get("proxy")
        .and_then(|p| p.get("api_key"))
        .and_then(YamlValue::as_str),
      Some("sekret")
    );
  }

  #[test]
  fn upsert_into_nonempty_flow_mapping_refuses() {
    // A *non-empty* hand-authored flow mapping can't take a block append
    // without corruption, so it's refused with a clear flow-style message.
    let err = upsert("proxy: {port: 11500}\n", &["proxy", "api_key"], "x").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("flow"), "error names flow style: {msg}");
  }

  #[test]
  fn render_value_tokens_round_trip_with_types() {
    assert_eq!(
      render_value(&YamlValue::String("/opt/x".into())).unwrap(),
      "/opt/x"
    );
    assert_eq!(render_value(&parse_ok("42")).unwrap(), "42");
    assert_eq!(render_value(&parse_ok("true")).unwrap(), "true");
    // A numeric-looking string must round-trip back to a string, not an int.
    let tok = render_value(&YamlValue::String("10000".into())).unwrap();
    assert_eq!(parse_ok(&tok).as_str(), Some("10000"), "stays a string");
    // A sequence renders as one-line flow that re-parses to the same list.
    let seq = render_value(&parse_ok("[/a, /b]")).unwrap();
    assert!(!seq.contains('\n'), "sequence stays single-line: {seq}");
    assert_eq!(parse_ok(&seq).as_sequence().map(|s| s.len()), Some(2));
  }

  #[test]
  fn remove_top_level_key_keeps_siblings() {
    let out = remove(
      "theme: latte\nllama_server_path: /opt/ls\n",
      &["llama_server_path"],
    )
    .unwrap()
    .expect("removed");
    let y = parse_ok(&out);
    assert!(y.get("llama_server_path").is_none());
    assert_eq!(y.get("theme").and_then(YamlValue::as_str), Some("latte"));
  }

  #[test]
  fn remove_prunes_now_empty_parent_block() {
    let out = remove(
      "theme: latte\nproxy:\n  api_key: x\n",
      &["proxy", "api_key"],
    )
    .unwrap()
    .expect("removed");
    let y = parse_ok(&out);
    assert!(y.get("proxy").is_none(), "now-empty proxy block is pruned");
    assert_eq!(y.get("theme").and_then(YamlValue::as_str), Some("latte"));
  }

  #[test]
  fn remove_absent_key_is_none() {
    assert!(remove("theme: latte\n", &["nope"]).unwrap().is_none());
  }
}
