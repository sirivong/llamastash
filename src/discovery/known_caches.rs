//! Resolve the set of scan roots discovery should walk by default,
//! merging user-configured paths with well-known cache locations
//! (HuggingFace hub, Ollama, LM Studio).
//!
//! Per-cache disables in `config.disable_default_cache_paths` let
//! users opt out of any single source without having to enumerate
//! the remaining roots by hand. A global `no_scan` flag (set by
//! `--no-scan` or `LLAMASTASH_NO_SCAN=1`) skips everything except
//! explicitly-passed `--model-path` roots, so agent invocations can
//! pin their scan surface (origin: R4).

use std::path::{Path, PathBuf};

use crate::config::CachePathsConfig;
use crate::discovery::lm_studio;
use crate::discovery::scanner::ScanRoot;
use crate::discovery::ModelSource;

/// Inputs to [`default_set`]. `user_paths` are unconditional — they
/// participate even when `no_scan` is set, since the user asked for
/// them explicitly. `disable` and `no_scan` shape the default cache
/// roots.
#[derive(Debug, Clone)]
pub struct RootResolution<'a> {
  pub user_paths: &'a [PathBuf],
  pub disable: &'a CachePathsConfig,
  pub no_scan: bool,
  /// Resolved `$HOME` (or equivalent). The platform default-cache
  /// paths are anchored under this. Discovery passes `dirs::home_dir`
  /// in production; tests pass a temp dir.
  pub home: Option<&'a Path>,
}

/// Merge user-configured roots with default cache roots into the
/// canonical ordered scan list. Duplicate paths are collapsed in
/// insertion order so a `--model-path` that overlaps an HF cache is
/// listed once (origin: R3 edge case).
pub fn default_set(res: RootResolution<'_>) -> Vec<ScanRoot> {
  let mut out: Vec<ScanRoot> = Vec::new();
  let mut seen = std::collections::BTreeSet::new();

  for p in res.user_paths {
    push_unique(
      &mut out,
      &mut seen,
      ScanRoot {
        path: canonical_or_raw(p),
        source: ModelSource::UserPath,
      },
    );
  }

  if res.no_scan {
    // Everything else is suppressed: agents that want deterministic
    // scope rely on this.
    return out;
  }

  let home = match res.home {
    Some(h) => h,
    None => return out,
  };

  if !res.disable.huggingface {
    for p in default_huggingface_paths(home) {
      push_unique(
        &mut out,
        &mut seen,
        ScanRoot {
          path: p,
          source: ModelSource::HuggingFace,
        },
      );
    }
  }
  if !res.disable.ollama {
    for p in default_ollama_paths(home) {
      push_unique(
        &mut out,
        &mut seen,
        ScanRoot {
          path: p,
          source: ModelSource::Ollama,
        },
      );
    }
  }
  if !res.disable.lm_studio {
    for p in default_lm_studio_paths(home) {
      push_unique(
        &mut out,
        &mut seen,
        ScanRoot {
          path: p,
          source: ModelSource::LmStudio,
        },
      );
    }
    // Honour the GUI-configured `paths.models` override the user set
    // in `~/.lmstudio/settings.json`. The resolver only returns
    // *existing* directories, so this won't generate phantom roots
    // when LM Studio isn't installed.
    for p in lm_studio::resolve_models_dirs(home) {
      push_unique(
        &mut out,
        &mut seen,
        ScanRoot {
          path: p,
          source: ModelSource::LmStudio,
        },
      );
    }
  }
  out
}

/// `$HOME/.cache/huggingface/hub` on Linux; `$HOME/Library/Caches/
/// huggingface/hub` on macOS. The `hub` directory is the one that
/// holds `models--<owner>--<repo>/snapshots/<rev>/*.gguf` trees.
pub fn default_huggingface_paths(home: &Path) -> Vec<PathBuf> {
  let mut paths = Vec::new();
  if cfg!(target_os = "macos") {
    paths.push(home.join("Library/Caches/huggingface/hub"));
  }
  // Linux default and the macOS-XDG override that some users set.
  paths.push(home.join(".cache/huggingface/hub"));
  paths
}

/// Ollama stores models under `$HOME/.ollama/models` by default; the
/// `OLLAMA_MODELS` env var (documented at
/// <https://github.com/ollama/ollama/blob/main/docs/faq.md#how-do-i-set-them-to-a-different-location>)
/// lets users relocate the cache to a roomier disk. We honour the env
/// var ahead of the default so a user with `OLLAMA_MODELS=/mnt/work/
/// ollama-models` (and no `~/.ollama/models`) sees their Ollama models
/// in the catalog.
///
/// Both paths are returned when set so a user who switched mid-flight
/// (leaving stale models in the home location) still sees the
/// historic set. `default_set`'s dedup keeps order stable.
///
/// The blob files are content-addressed (hash-named); the scanner
/// won't pick them up under a `.gguf` extension filter on its own —
/// the dedicated `ollama` enumerator handles that wiring.
pub fn default_ollama_paths(home: &Path) -> Vec<PathBuf> {
  let mut paths = Vec::with_capacity(2);
  if let Some(env) = std::env::var_os("OLLAMA_MODELS") {
    if !env.is_empty() {
      paths.push(PathBuf::from(env));
    }
  }
  paths.push(home.join(".ollama/models"));
  paths
}

/// LM Studio's defaults across platforms. Plan: probe `~/.lmstudio/
/// models` (the documented location), then `~/.cache/lm-studio/
/// models` (older installs). A future enhancement reads `~/
/// .lmstudio/settings.json` for the user's configured override.
pub fn default_lm_studio_paths(home: &Path) -> Vec<PathBuf> {
  vec![
    home.join(".lmstudio/models"),
    home.join(".cache/lm-studio/models"),
  ]
}

fn canonical_or_raw(p: &Path) -> PathBuf {
  std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn push_unique(
  out: &mut Vec<ScanRoot>,
  seen: &mut std::collections::BTreeSet<PathBuf>,
  root: ScanRoot,
) {
  if seen.insert(root.path.clone()) {
    out.push(root);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn disable_default() -> CachePathsConfig {
    CachePathsConfig {
      huggingface: false,
      ollama: false,
      lm_studio: false,
    }
  }

  #[test]
  fn no_scan_keeps_only_user_paths() {
    let user = vec![PathBuf::from("/explicit/user/path")];
    let home = PathBuf::from("/home/user");
    let roots = default_set(RootResolution {
      user_paths: &user,
      disable: &disable_default(),
      no_scan: true,
      home: Some(&home),
    });
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].source, ModelSource::UserPath);
  }

  #[test]
  fn default_includes_all_three_caches_when_enabled() {
    let user: Vec<PathBuf> = Vec::new();
    let home = PathBuf::from("/home/user");
    let roots = default_set(RootResolution {
      user_paths: &user,
      disable: &disable_default(),
      no_scan: false,
      home: Some(&home),
    });
    let sources: std::collections::BTreeSet<_> = roots.iter().map(|r| r.source).collect();
    assert!(sources.contains(&ModelSource::HuggingFace));
    assert!(sources.contains(&ModelSource::Ollama));
    assert!(sources.contains(&ModelSource::LmStudio));
  }

  #[test]
  fn per_cache_disable_drops_only_that_source() {
    let user: Vec<PathBuf> = Vec::new();
    let home = PathBuf::from("/home/user");
    let mut disable = disable_default();
    disable.ollama = true;
    let roots = default_set(RootResolution {
      user_paths: &user,
      disable: &disable,
      no_scan: false,
      home: Some(&home),
    });
    let sources: std::collections::BTreeSet<_> = roots.iter().map(|r| r.source).collect();
    assert!(sources.contains(&ModelSource::HuggingFace));
    assert!(!sources.contains(&ModelSource::Ollama), "ollama disabled");
    assert!(sources.contains(&ModelSource::LmStudio));
  }

  #[test]
  fn ollama_paths_honor_env_var_with_home_default_as_fallback() {
    // Single test for both scenarios so two parallel test threads
    // don't race on the shared OLLAMA_MODELS env var. Serialised
    // through `cli::test_lock` so other env-mutating tests in the
    // suite don't trample this one mid-flight either.
    let _lock = crate::cli::test_lock::serialize();
    let saved = std::env::var_os("OLLAMA_MODELS");

    // Env set → env path comes first, home default still appears
    // (so a user mid-migration with stale models in `~/.ollama/models`
    // doesn't lose visibility).
    std::env::set_var("OLLAMA_MODELS", "/mnt/ollama-models");
    let paths = default_ollama_paths(Path::new("/home/user"));
    assert_eq!(
      paths.first().map(PathBuf::as_path),
      Some(Path::new("/mnt/ollama-models"))
    );
    assert!(
      paths.contains(&PathBuf::from("/home/user/.ollama/models")),
      "home default must still appear: {paths:?}"
    );

    // Env unset → home default only.
    std::env::remove_var("OLLAMA_MODELS");
    assert_eq!(
      default_ollama_paths(Path::new("/home/user")),
      vec![PathBuf::from("/home/user/.ollama/models")]
    );

    // Env present but empty → treated as unset.
    std::env::set_var("OLLAMA_MODELS", "");
    assert_eq!(
      default_ollama_paths(Path::new("/home/user")),
      vec![PathBuf::from("/home/user/.ollama/models")]
    );

    match saved {
      Some(s) => std::env::set_var("OLLAMA_MODELS", s),
      None => std::env::remove_var("OLLAMA_MODELS"),
    }
  }

  #[test]
  fn user_path_overlapping_a_default_cache_dedupes() {
    // The hf-hub default lives under ~/.cache/huggingface/hub. If the
    // user passes that exact path via --model-path, we want it listed
    // once, not once-as-user-and-once-as-huggingface.
    let user = vec![PathBuf::from("/home/user/.cache/huggingface/hub")];
    let home = PathBuf::from("/home/user");
    let roots = default_set(RootResolution {
      user_paths: &user,
      disable: &disable_default(),
      no_scan: false,
      home: Some(&home),
    });
    let count_for_path: usize = roots
      .iter()
      .filter(|r| r.path == Path::new("/home/user/.cache/huggingface/hub"))
      .count();
    assert_eq!(count_for_path, 1);
    // The user-supplied entry wins because it was inserted first.
    assert_eq!(roots[0].source, ModelSource::UserPath);
  }

  #[test]
  fn missing_home_dir_yields_only_user_paths() {
    let user = vec![PathBuf::from("/explicit/path")];
    let roots = default_set(RootResolution {
      user_paths: &user,
      disable: &disable_default(),
      no_scan: false,
      home: None,
    });
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].source, ModelSource::UserPath);
  }

  #[test]
  fn lm_studio_settings_override_surfaces_as_lm_studio_root() {
    // Plan: when a real LM Studio install advertises a non-default
    // `paths.models` in `~/.lmstudio/settings.json`, that directory
    // must show up among the LM Studio roots — not get silently
    // dropped in favour of the hard-coded `~/.lmstudio/models`.
    use std::fs;
    let home = std::env::temp_dir().join(format!(
      "llamastash-known-caches-lmstudio-{}-{}",
      std::process::id(),
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
    ));
    fs::create_dir_all(home.join(".lmstudio")).unwrap();
    let custom = home.join("Models/LmStudio");
    fs::create_dir_all(&custom).unwrap();
    let settings = serde_json::json!({"paths": {"models": custom.to_string_lossy()}});
    fs::write(
      home.join(".lmstudio/settings.json"),
      serde_json::to_vec(&settings).unwrap(),
    )
    .unwrap();

    let user: Vec<PathBuf> = Vec::new();
    let roots = default_set(RootResolution {
      user_paths: &user,
      disable: &disable_default(),
      no_scan: false,
      home: Some(&home),
    });
    let lm_paths: Vec<&Path> = roots
      .iter()
      .filter(|r| r.source == ModelSource::LmStudio)
      .map(|r| r.path.as_path())
      .collect();
    assert!(
      lm_paths.contains(&custom.as_path()),
      "LM Studio override `{}` must appear among roots, got {:?}",
      custom.display(),
      lm_paths
    );
    fs::remove_dir_all(&home).ok();
  }
}
