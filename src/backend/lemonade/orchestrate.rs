//! Managed-multiplexer launch orchestration for Lemonade.
//!
//! llama.cpp spawns one supervised child *per model*. Lemonade is different:
//! one long-lived `lemond` umbrella serves every model behind its API
//! (R10). This module owns the umbrella's lifecycle — ensuring exactly one
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
//! Per-model routing (a Lemonade-backed model appearing in the catalog and
//! the proxy forwarding to the umbrella's port) is wired separately — it
//! depends on the catalog `backend` tag and proxy-target work. Until that
//! lands, [`crate::ipc`]'s `start_model` does not drive this path; the
//! orchestration here is exercised directly by its integration test.

use std::path::{Path, PathBuf};

use crate::backend::ProcessLaunchSpec;
use crate::daemon::registry::{LaunchId, SupervisorRegistry};
use crate::daemon::supervisor::{spawn, LaunchOrigin, ManagedModel, ManagedSpawn, SpawnError};
use crate::gguf::identity::ModelId;
use crate::launch::mode::LaunchMode;
use crate::launch::params::LaunchParams;

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
/// Note: the existence check + spawn are not yet atomic; two concurrent
/// first-callers could both spawn. Single-flight hardening (reuse the
/// registry's port-reservation CAS) is a follow-up. The current live caller
/// is the sequential `start_model` path.
pub async fn ensure_umbrella(
  registry: &SupervisorRegistry,
  port: u16,
  umbrella: ProcessLaunchSpec,
  log_path: PathBuf,
) -> Result<ManagedModel, SpawnError> {
  let id = umbrella_launch_id();
  if let Some(existing) = registry.get(&id).await {
    return Ok(existing);
  }
  let model = spawn(ManagedSpawn {
    id: umbrella_model_id(&umbrella.binary),
    params: LaunchParams::new(umbrella.binary.clone(), LaunchMode::Chat),
    port,
    mode: LaunchMode::Chat,
    log_path,
    plan: umbrella,
    origin: LaunchOrigin::Manual,
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
