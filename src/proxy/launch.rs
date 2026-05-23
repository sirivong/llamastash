//! Proxy-side launch helper.
//!
//! When a `/v1/...` request lands for a model that exists in the
//! catalog but has no Ready supervisor, [`auto_start`] drives the
//! launch in-process by calling
//! [`crate::ipc::methods::start_model_inner`] — the same composition
//! pipeline the IPC `start_model` handler uses, so the two paths
//! can't drift apart.
//!
//! The flow:
//!   1. Build a default [`StartParams`] from the resolved catalog
//!      row (just the path; mode defaults to Chat, no port
//!      preference, no caller knobs — `start_model_inner` then
//!      replays the same `last_params → arch_defaults → built-in`
//!      cascade the IPC handler does).
//!   2. Acquire single-flight rights via
//!      [`crate::proxy::coalesce::Coalesce::acquire`]. Leaders run
//!      `start_model_inner`; followers `.wait()` and receive the
//!      leader's outcome directly from the slot (no re-snapshot).
//!   3. Poll [`crate::daemon::supervisor::ManagedModel::state`] at
//!      100 ms cadence until it reaches `Ready` (forward) or
//!      `Error{cause}` (fallback). No client-facing timeout — per
//!      the locked Key Decision "Hard supervisor Error only; wait
//!      indefinitely on Loading."
//!
//! Plan: docs/plans/2026-05-21-001-feat-proxy-router-plan.md (Unit 4).

use std::sync::Arc;
use std::time::Duration;

use crate::cli::resolve::CatalogRow;
use crate::daemon::supervisor::ManagedState;
use crate::gguf::identity::ModelId;
use crate::ipc::methods::{start_model_inner, StartParams};

use super::coalesce::{AcquireOutcome, SharedOutcome};
use super::state::ProxyState;

/// Outcome of [`auto_start`]. The proxy's caller branches on this:
/// `Ready` forwards against `(port, model_id)`; `Failed` enters the
/// family-MRU fallback path.
#[derive(Clone, Debug)]
pub(crate) enum LaunchOutcome {
  /// Supervisor reached `ManagedState::Ready`. The caller forwards
  /// against `port`; `model_id` is threaded so the forward path can
  /// re-verify the supervisor still owns `port` before sending
  /// (port-reuse defense — see `super::forward`).
  Ready { port: u16, model_id: ModelId },
  /// Launch hit a terminal error before reaching Ready. `cause`
  /// surfaces in the 503 `launch_failed` JSON body when no fallback
  /// is available.
  Failed { cause: String },
}

impl From<SharedOutcome> for LaunchOutcome {
  fn from(s: SharedOutcome) -> Self {
    match s {
      SharedOutcome::Ready { port, model_id } => LaunchOutcome::Ready { port, model_id },
      SharedOutcome::Failed { cause } => LaunchOutcome::Failed { cause },
    }
  }
}

impl From<LaunchOutcome> for SharedOutcome {
  fn from(o: LaunchOutcome) -> Self {
    match o {
      LaunchOutcome::Ready { port, model_id } => SharedOutcome::Ready { port, model_id },
      LaunchOutcome::Failed { cause } => SharedOutcome::Failed { cause },
    }
  }
}

/// Drive a launch (or wait on an in-flight one) and resolve to a
/// port once Ready. Returns [`LaunchOutcome::Failed`] if the
/// supervisor reaches `Error{cause}` before Ready, or if a follower
/// observed the leader's launch failure.
///
/// The proxy must hold `Arc<ProxyState>` for the duration so the
/// coalesce + supervisor handles stay alive across the await
/// points.
pub(crate) async fn auto_start(state: &Arc<ProxyState>, resolved: &CatalogRow) -> LaunchOutcome {
  // Compute the canonical ModelId from the resolved row. We read
  // the header here rather than trusting any in-process cache so
  // the single-flight key matches what `start_model_inner` will
  // observe at spawn time (it does the same read internally).
  //
  // The header read is up to 16 MiB of synchronous I/O; offload to
  // a blocking thread so we don't stall the tokio worker.
  let row = resolved.clone();
  let model_id = match tokio::task::spawn_blocking(move || canonical_id_for_row(&row)).await {
    Ok(Ok(id)) => id,
    Ok(Err(cause)) => return LaunchOutcome::Failed { cause },
    Err(join) => {
      return LaunchOutcome::Failed {
        cause: format!("GGUF header read panicked: {join}"),
      };
    }
  };

  // Single-flight acquire. Leaders run the launch and stamp the
  // outcome on the slot; followers read the outcome directly when
  // the leader finishes (or wake to `None` on cancellation).
  match state.coalesce.acquire(model_id.clone()).await {
    AcquireOutcome::Leader(leader) => {
      let outcome = drive_launch_as_leader(state, resolved, model_id).await;
      leader.finish(outcome.clone().into()).await;
      outcome
    }
    AcquireOutcome::Follower(follower) => match follower.wait().await {
      Some(shared) => shared.into(),
      None => LaunchOutcome::Failed {
        cause: "leader launch cancelled".to_string(),
      },
    },
  }
}

/// Run [`start_model_inner`], then poll `state()` at 100 ms until
/// the supervisor reaches `Ready` or `Error`. Pulled out so the
/// leader arm of [`auto_start`] reads top-to-bottom without nesting.
async fn drive_launch_as_leader(
  state: &Arc<ProxyState>,
  resolved: &CatalogRow,
  model_id: ModelId,
) -> LaunchOutcome {
  let params = StartParams {
    model_path: std::path::PathBuf::from(&resolved.path),
    ..StartParams::default()
  };
  let started = match start_model_inner(&state.ctx, params).await {
    Ok(s) => s,
    Err(e) => {
      return LaunchOutcome::Failed {
        cause: format!("start_model_inner: {}", e.message),
      };
    }
  };

  // Poll the supervisor state machine. 100 ms cadence per the Key
  // Decision; no client-facing timeout — only `Error{cause}` and
  // `Stopping` trigger fallback (Loading waits indefinitely).
  loop {
    match started.model.state().await {
      ManagedState::Ready => {
        return LaunchOutcome::Ready {
          port: started.port,
          model_id,
        };
      }
      ManagedState::Error { cause } => {
        return LaunchOutcome::Failed { cause };
      }
      ManagedState::Stopped => {
        return LaunchOutcome::Failed {
          cause: "supervisor exited before reaching Ready".to_string(),
        };
      }
      ManagedState::Stopping => {
        return LaunchOutcome::Failed {
          cause: "model stopped while launching".to_string(),
        };
      }
      ManagedState::Launching | ManagedState::Loading => {
        tokio::time::sleep(Duration::from_millis(100)).await;
      }
    }
  }
}

/// Compute the canonical [`ModelId`] for a resolved [`CatalogRow`].
/// Synchronous — call via `spawn_blocking` to keep the async worker
/// thread free.
fn canonical_id_for_row(row: &CatalogRow) -> Result<ModelId, String> {
  let path = std::path::Path::new(&row.path);
  // Path is omitted from the error string so it does not leak into
  // the 503 `launch_failed` response body. The daemon log still
  // carries the path via the wrapped IoError on the supervisor side.
  let header =
    crate::gguf::header::read_path(path, crate::gguf::header::HeaderReadOptions::default())
      .map_err(|e| format!("could not read GGUF header: {e}"))?;
  Ok(crate::gguf::identity::compute(path, &header.raw))
}
