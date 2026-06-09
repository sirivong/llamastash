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
pub mod shard_sizes;
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
  /// Multimodal capability of the model's auto-detected mmproj projector
  /// companion (vision / audio), or `None` when no projector was found.
  /// Derived from the projector GGUF's `clip.has_*_encoder` keys by the
  /// scanner (see [`scanner::find_mmproj`]). Surfaced after the model
  /// title in the TUI so the user knows a model launches multimodal.
  pub multimodal: Option<Multimodal>,
}

/// Multimodal modality a model's mmproj projector advertises. A
/// projector can be vision-only, audio-only, or both (an "omni" model),
/// so the two flags are independent rather than an enum. Derived from
/// the projector GGUF's `clip.has_vision_encoder` / `clip.has_audio_encoder`
/// metadata keys (llama.cpp clip convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Multimodal {
  pub vision: bool,
  pub audio: bool,
}

impl Multimodal {
  /// `(glyph, description)` for every modality the header can render —
  /// single source of truth shared by the right-pane title glyphs and
  /// the help-overlay Legend so the two never drift. Glyphs are
  /// single-cell BMP symbols matching the `status_icons` house style.
  pub const LEGEND: [(char, &'static str); 2] = [
    ('◉', "vision (multimodal projector)"),
    ('♪', "audio (multimodal projector)"),
  ];

  /// Glyphs for the modalities this projector actually advertises, in
  /// `LEGEND` order. Empty when neither flag is set.
  pub fn glyphs(&self) -> Vec<char> {
    let mut out = Vec::new();
    if self.vision {
      out.push(Self::LEGEND[0].0);
    }
    if self.audio {
      out.push(Self::LEGEND[1].0);
    }
    out
  }
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
  // A backend-registry source (e.g. a managed-multiplexer engine) adds a
  // variant here + arms in `label` / `backend_id`.
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

  /// The id of the backend that serves models from this source (R13/R14).
  ///
  /// Disk sources (user / HF / Ollama / LM Studio) are all local GGUF files
  /// served by the direct llama.cpp backend. A backend-registry source adds
  /// its own arm returning that backend's id.
  pub fn backend_id(&self) -> &'static str {
    match self {
      ModelSource::UserPath
      | ModelSource::HuggingFace
      | ModelSource::Ollama
      | ModelSource::LmStudio => "llamacpp",
    }
  }
}
