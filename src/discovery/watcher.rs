//! Live-update the discovered-model list via a debounced filesystem
//! watcher (origin: R22).
//!
//! `notify-debouncer-mini` coalesces rapid bursts (e.g., copying a
//! split-shard set, or `hf-hub` writing many `.part` files in quick
//! succession) into one event per quiet window — 500 ms by default
//! per the plan. Each event surfaces to the caller as a [`WatchEvent`]
//! over an `mpsc::Receiver`; the daemon's discovery task consumes
//! these and re-runs the affected scan slice to refresh
//! `list_models`.
//!
//! A 5-minute `tokio::time::interval` periodic rescan tick rides
//! alongside as a backstop: deeply-nested cache trees (HuggingFace
//! hub) can drop events under load, and a missed `.gguf` should not
//! mean a permanently invisible model. The tick fires on the same
//! channel with [`WatchEvent::PeriodicRescan`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use notify_debouncer_mini::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, Debouncer};
use tokio::sync::mpsc;

/// What the watcher reports. Consumers don't need to distinguish
/// create/modify/delete at this layer — the discovery task re-runs the
/// scanner over the affected root regardless.
#[derive(Debug, Clone)]
pub enum WatchEvent {
  /// Filesystem activity under one of the watched roots. `paths`
  /// lists every path the debouncer collected within the quiet
  /// window; consumers can target a re-scan to the impacted dirs.
  Changed { paths: Vec<PathBuf> },
  /// 5-minute periodic backstop. Consumers should re-walk every
  /// watched root in case the OS dropped an event under load.
  PeriodicRescan,
}

/// Tunables. Production defaults match the plan: 500 ms debounce,
/// 5-minute periodic backstop. Tests shorten them for responsiveness.
#[derive(Debug, Clone, Copy)]
pub struct WatcherOptions {
  pub debounce: Duration,
  pub periodic_rescan: Duration,
  /// Buffer for the outbound event channel. Defaults to 64 so a slow
  /// consumer doesn't starve the watcher thread.
  pub channel_capacity: usize,
}

impl Default for WatcherOptions {
  fn default() -> Self {
    Self {
      debounce: Duration::from_millis(500),
      periodic_rescan: Duration::from_secs(5 * 60),
      channel_capacity: 64,
    }
  }
}

/// Handle that keeps the debouncer alive. Dropping it stops the
/// filesystem watcher and the periodic-rescan task; in-flight events
/// already on the channel are still deliverable.
pub struct WatcherHandle {
  _debouncer: Debouncer<RecommendedWatcher>,
  _periodic_task: tokio::task::JoinHandle<()>,
}

/// Begin watching `roots`. Returns a receiver that yields
/// [`WatchEvent`]s and a handle that must be retained for the watcher
/// to keep running.
///
/// Roots that don't exist (or aren't readable) are logged and
/// skipped — discovery should still surface events for the remaining
/// roots. An empty roots list yields a receiver that only ever
/// produces [`WatchEvent::PeriodicRescan`] ticks, which is the
/// degenerate "no scan paths configured" shape.
pub fn start(
  roots: Vec<PathBuf>,
  opts: WatcherOptions,
) -> Result<(WatcherHandle, mpsc::Receiver<WatchEvent>), notify_debouncer_mini::notify::Error> {
  let (tx, rx) = mpsc::channel(opts.channel_capacity);

  let tx_for_debouncer = tx.clone();
  let mut debouncer = new_debouncer(opts.debounce, move |res: DebounceEventResult| match res {
    Ok(events) => {
      let paths: Vec<PathBuf> = events.into_iter().map(|e| e.path).collect();
      if paths.is_empty() {
        return;
      }
      // Channel send from a sync (debouncer-owned) thread: best-effort
      // non-blocking. If the consumer is overwhelmed, we drop the
      // event — the periodic rescan tick is the safety net.
      if let Err(e) = tx_for_debouncer.blocking_send(WatchEvent::Changed { paths }) {
        log::debug!("watcher channel closed mid-event: {e}");
      }
    }
    Err(err) => {
      log::warn!("filesystem watcher error: {err}");
    }
  })?;

  for root in &roots {
    if !root.exists() {
      log::warn!("watcher: root does not exist, skipping: {}", root.display());
      continue;
    }
    if let Err(e) = debouncer.watcher().watch(root, RecursiveMode::Recursive) {
      log::warn!("watcher: cannot watch {}: {e}", root.display());
    }
  }

  // Periodic rescan tick. A `tokio::time::interval` fires roughly on
  // the configured cadence; missed ticks coalesce so a paused
  // consumer doesn't get a flurry on resume.
  let tx_for_periodic = tx;
  let periodic_period = opts.periodic_rescan;
  let periodic_task = tokio::spawn(async move {
    let mut ticker = tokio::time::interval(periodic_period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Skip the immediate first tick — callers do the initial scan
    // themselves when they wire the watcher up.
    ticker.tick().await;
    loop {
      ticker.tick().await;
      if tx_for_periodic
        .send(WatchEvent::PeriodicRescan)
        .await
        .is_err()
      {
        return;
      }
    }
  });

  Ok((
    WatcherHandle {
      _debouncer: debouncer,
      _periodic_task: periodic_task,
    },
    rx,
  ))
}

/// Convenience: filter a [`WatchEvent::Changed`]'s paths down to just
/// those whose extension is `.gguf` (live `.part` files and other
/// noise drop out). Returns an empty vec for other event variants.
pub fn changed_gguf_paths(event: &WatchEvent) -> Vec<&Path> {
  match event {
    WatchEvent::Changed { paths } => paths
      .iter()
      .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("gguf"))
      .map(PathBuf::as_path)
      .collect(),
    WatchEvent::PeriodicRescan => Vec::new(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use std::fs;
  use std::time::{SystemTime, UNIX_EPOCH};

  fn temp_root(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .expect("clock")
      .as_nanos();
    let p = std::env::temp_dir().join(format!(
      "llamatui-watcher-{label}-{}-{nanos}",
      std::process::id()
    ));
    fs::create_dir_all(&p).expect("temp root");
    p
  }

  fn fast_opts() -> WatcherOptions {
    WatcherOptions {
      debounce: Duration::from_millis(50),
      periodic_rescan: Duration::from_millis(150),
      channel_capacity: 16,
    }
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn changed_event_fires_when_gguf_lands_in_watched_root() {
    let root = temp_root("change");
    let (_handle, mut rx) = start(vec![root.clone()], fast_opts()).expect("start watcher");

    // Drop a file *after* the watcher is wired up.
    let gguf = root.join("dropped.gguf");
    fs::write(&gguf, b"GGUF\x03\x00\x00\x00").unwrap();

    let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
      .await
      .expect("watcher emits within 2s")
      .expect("channel still open");
    match event {
      WatchEvent::Changed { paths } => {
        assert!(
          paths.iter().any(|p| p.ends_with("dropped.gguf")),
          "expected dropped.gguf in event, got {paths:?}"
        );
      }
      WatchEvent::PeriodicRescan => {
        // Periodic ticks may interleave on slow machines — fish for
        // the actual change event.
        let next = tokio::time::timeout(Duration::from_secs(2), rx.recv())
          .await
          .expect("second event within 2s")
          .expect("channel open");
        match next {
          WatchEvent::Changed { paths } => assert!(
            paths.iter().any(|p| p.ends_with("dropped.gguf")),
            "expected dropped.gguf, got {paths:?}"
          ),
          other => panic!("expected Changed after PeriodicRescan, got {other:?}"),
        }
      }
    }
    fs::remove_dir_all(&root).ok();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn periodic_rescan_fires_on_its_own() {
    let root = temp_root("periodic");
    let (_handle, mut rx) = start(vec![root.clone()], fast_opts()).expect("start watcher");

    // Wait long enough that at least one periodic tick must arrive.
    let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
      .await
      .expect("periodic rescan within 2s")
      .expect("channel open");
    assert!(matches!(event, WatchEvent::PeriodicRescan));
    fs::remove_dir_all(&root).ok();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn missing_root_is_logged_and_skipped_without_failure() {
    let alive = temp_root("alive");
    let dead = PathBuf::from("/nonexistent/llamatui/watcher/root");
    let (_handle, _rx) =
      start(vec![dead, alive.clone()], fast_opts()).expect("missing root must not error");
    fs::remove_dir_all(&alive).ok();
  }

  #[test]
  fn changed_gguf_paths_filters_to_gguf_extension() {
    let event = WatchEvent::Changed {
      paths: vec![
        PathBuf::from("/a/model.gguf"),
        PathBuf::from("/a/model.gguf.part"),
        PathBuf::from("/a/notes.txt"),
      ],
    };
    let filtered: Vec<_> = changed_gguf_paths(&event).into_iter().collect();
    assert_eq!(filtered.len(), 1);
    assert!(filtered[0].ends_with("model.gguf"));
    // Periodic rescan never carries paths.
    assert!(changed_gguf_paths(&WatchEvent::PeriodicRescan).is_empty());
  }
}
