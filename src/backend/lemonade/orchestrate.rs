//! Managed-multiplexer launch orchestration for Lemonade.
//!
//! llama.cpp spawns one supervised child *per model*. Lemonade is different:
//! one long-lived `lemond` umbrella serves every model behind its API
//! This module owns the umbrella's lifecycle — ensuring exactly one
//! `lemond` is supervised and ready before any per-model work is delegated
//! to it.
//!
//! The umbrella reuses the **generic** [`crate::daemon::supervisor`]: it is
//! just a [`ProcessLaunchSpec`] whose
//! readiness is `lemond`'s `/live` endpoint. Because the supervisor keys a
//! [`ManagedModel`] by a [`crate::gguf::identity::ModelId`], the umbrella —
//! which is not a GGUF model — is registered under a synthetic id (path =
//! the `lemond` binary, sentinel header hash) and a reserved
//! [`umbrella_launch_id`].
//!
//! The umbrella is brought up from two callers, both via [`ensure_umbrella`]:
//! the daemon's boot-time supervision (when the Lemonade backend is enabled)
//! and the per-model `start_model` path. Per-model routing reads the catalog
//! `backend` tag and forwards through the proxy to the umbrella's port.

use std::path::{Path, PathBuf};

use tokio::sync::Mutex;

use crate::backend::ProcessLaunchSpec;
use crate::daemon::registry::{LaunchId, SupervisorRegistry};
use crate::daemon::supervisor::{spawn, LaunchOrigin, ManagedModel, ManagedSpawn, SpawnError};
use crate::gguf::identity::ModelId;
use crate::launch::mode::LaunchMode;
use crate::launch::params::LaunchParams;

/// Process-wide single-flight guard for [`ensure_umbrella`]. Serializes the
/// existence-check + spawn so two concurrent first-callers (e.g. boot
/// supervision racing an early `start_model`) can't both spawn an umbrella.
static ENSURE_LOCK: Mutex<()> = Mutex::const_new(());

/// Reserved supervisor id for the single `lemond` umbrella. One umbrella
/// per daemon, shared by all Lemonade-backed models.
pub fn umbrella_launch_id() -> LaunchId {
  LaunchId("lemonade-umbrella".to_string())
}

/// Synthetic [`ModelId`] for the umbrella. It is not a GGUF model, but the
/// supervisor keys every [`ManagedModel`] by `ModelId`; using the `lemond`
/// binary path keeps status output legible, and a fixed sentinel header
/// hash distinguishes it from any real model.
fn umbrella_model_id(binary: &Path) -> ModelId {
  ModelId {
    path: binary.to_path_buf(),
    header_blake3: [0u8; 32],
  }
}

/// Ensure the `lemond` umbrella is supervised and ready, returning its
/// [`ManagedModel`] handle. **Idempotent**: if an umbrella is already
/// registered under [`umbrella_launch_id`], it is reused and `port` +
/// `umbrella` are ignored (one umbrella per daemon).
///
/// `port` is the loopback port the umbrella's [`ProcessLaunchSpec`] was
/// built to bind (the supervisor probes it for `/live` readiness). On a
/// fresh spawn the supervisor blocks until the umbrella reports ready or
/// the probe times out (surfaced as [`SpawnError`]).
///
/// The existence-check + spawn run under a process-wide lock so concurrent
/// first-callers serialize: the first spawns, the rest observe the registered
/// umbrella and reuse it (single-flight).
pub async fn ensure_umbrella(
  registry: &SupervisorRegistry,
  port: u16,
  umbrella: ProcessLaunchSpec,
  log_path: PathBuf,
) -> Result<ManagedModel, SpawnError> {
  let _guard = ENSURE_LOCK.lock().await;
  let id = umbrella_launch_id();
  if let Some(existing) = registry.get(&id).await {
    return Ok(existing);
  }
  // Not yet supervised, so we must bind `port` ourselves to start the umbrella.
  // If another process already holds it (a hand-started `lemond`, a stale
  // instance), our spawned child would lose the bind race while the foreign
  // process keeps answering the `/live` probe — a false "ready". Refuse with a
  // clear error instead of adopting a process we don't own. Checked *after* the
  // reuse path (under the single-flight lock) so our own already-running
  // umbrella, which legitimately holds the port, is never rejected.
  if !super::umbrella_port_available(port) {
    return Err(SpawnError::PortInUse(port));
  }
  let model = spawn(ManagedSpawn {
    id: umbrella_model_id(&umbrella.binary),
    params: LaunchParams::new(umbrella.binary.clone(), LaunchMode::Chat),
    port,
    mode: LaunchMode::Chat,
    log_path,
    plan: umbrella,
    origin: LaunchOrigin::Manual,
    // The Lemonade umbrella has no `--fit` ctx semantics; the strict-fit
    // gate never applies to it.
    fit_gate: None,
  })
  .await?;
  registry.insert(id, model.clone()).await;
  Ok(model)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn umbrella_id_is_stable_and_reserved() {
    assert_eq!(umbrella_launch_id().as_str(), "lemonade-umbrella");
  }

  #[test]
  fn umbrella_model_id_uses_binary_path_and_sentinel_hash() {
    let id = umbrella_model_id(Path::new("/opt/lemonade/lemond"));
    assert_eq!(id.path, PathBuf::from("/opt/lemonade/lemond"));
    assert_eq!(id.header_blake3, [0u8; 32]);
  }
}
