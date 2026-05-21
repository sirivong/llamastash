//! Filesystem discovery: scan roots for `.gguf` files, group split-shard
//! sets, surface them on a streaming `mpsc::Receiver` so the TUI can
//! render rows as they arrive (origin: R1, R9).
//!
//! Module layout mirrors the responsibilities in the v1 plan:
//! - [`scanner`] — walk one or more roots and emit `DiscoveredModel`s.
//! - [`split_gguf`] — collapse `*-NNNNN-of-MMMMM.gguf` sets into one
//!   user-visible entry (R5).
//!
//! Cache-aware enumerators (HuggingFace, Ollama, LM Studio), the
//! filesystem watcher, and the HuggingFace pull worker land in later
//! commits within Unit 4.

pub mod catalog;
pub mod known_caches;
pub mod lm_studio;
pub mod metadata_cache;
pub mod ollama;
pub mod scanner;
pub mod split_gguf;
pub mod watcher;

pub use catalog::ModelCatalog;
pub use metadata_cache::MetadataCache;

use std::path::PathBuf;

use crate::gguf::metadata::ModelMetadata;

/// A model surfaced by discovery. Sent over the scanner's mpsc channel
/// so the TUI can render rows incrementally rather than waiting for
/// the whole scan to finish.
///
/// `metadata` is `None` when the GGUF header parse failed (truncated,
/// bad magic, unsupported version, …); discovery still surfaces the
/// row with a warning glyph rather than dropping the file (origin:
/// Unit 4 edge case "empty file with `.gguf` extension").
#[derive(Debug, Clone)]
pub struct DiscoveredModel {
  /// Canonical absolute path to the launchable file. For split-shard
  /// sets this is shard 1; for singles it's the file itself.
  pub path: PathBuf,
  /// Directory the file (or shard set) lives in. Discovery groups by
  /// parent for the TUI's "Models / `dir`" rendering.
  pub parent: PathBuf,
  /// Source cache the file was discovered through, if any. `None` for
  /// user-configured `--model-path` roots that don't match a known
  /// cache layout.
  pub source: ModelSource,
  /// Parsed GGUF metadata, or `None` if the header parse failed.
  pub metadata: Option<ModelMetadata>,
  /// Diagnostic surfaced when `metadata` is `None`. The TUI renders a
  /// warning glyph and the user can hover to see the cause.
  pub parse_error: Option<String>,
  /// Sibling shards when this entry represents a split-GGUF set.
  /// Empty for a single-file model.
  pub split_siblings: Vec<PathBuf>,
  /// Source-supplied human label preferred over `path.file_stem()` by
  /// the TUI / CLI display layers. Ollama populates this with the
  /// resolved `<name>:<tag>` so users see `gemma4:e2b` instead of the
  /// content-addressed blob hash. `None` for sources where the file
  /// stem is already meaningful (HF cache, LM Studio, user paths).
  pub display_label: Option<String>,
}

/// Provenance label for a discovered model. The TUI groups by source
/// in the left pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModelSource {
  /// A user-configured `--model-path` or config-level scan root.
  UserPath,
  /// A HuggingFace hub cache directory.
  HuggingFace,
  /// An Ollama blob/manifest cache.
  Ollama,
  /// An LM Studio models directory.
  LmStudio,
}

impl ModelSource {
  pub fn label(&self) -> &'static str {
    match self {
      ModelSource::UserPath => "user",
      ModelSource::HuggingFace => "huggingface",
      ModelSource::Ollama => "ollama",
      ModelSource::LmStudio => "lm-studio",
    }
  }
}
