//! In-memory registry of every active `ManagedModel`.
//!
//! Lives on the `MethodContext` so IPC handlers can look up
//! supervisors by `LaunchId` and the daemon can iterate the map for
//! `status` / `stop_all`. The registry intentionally keys on a
//! monotonically-increasing `LaunchId` rather than `ModelId` so the
//! same GGUF can be launched twice (different ports, different
//! purposes) without collisions.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::daemon::supervisor::ManagedModel;

/// Stable identifier for one launch. Strings on the wire so future
/// schemes (UUID, etc.) don't require an IPC bump.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LaunchId(pub String);

impl LaunchId {
  pub fn from_counter(n: u64) -> Self {
    Self(format!("L{n}"))
  }

  pub fn as_str(&self) -> &str {
    &self.0
  }
}

/// Shared, cheap-to-clone registry of supervisors. `Arc<RwLock<…>>`
/// inside, mirroring `ModelCatalog`'s pattern so IPC handlers have a
/// consistent shape.
#[derive(Debug, Clone, Default)]
pub struct SupervisorRegistry {
  inner: Arc<RwLock<BTreeMap<LaunchId, ManagedModel>>>,
  counter: Arc<AtomicU64>,
}

impl SupervisorRegistry {
  pub fn new() -> Self {
    Self::default()
  }

  /// Generate the next launch id. Monotonic per daemon process, so
  /// IDs are unique within one daemon lifetime.
  pub fn next_id(&self) -> LaunchId {
    let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
    LaunchId::from_counter(n)
  }

  pub async fn insert(&self, id: LaunchId, model: ManagedModel) {
    self.inner.write().await.insert(id, model);
  }

  pub async fn get(&self, id: &LaunchId) -> Option<ManagedModel> {
    self.inner.read().await.get(id).cloned()
  }

  pub async fn remove(&self, id: &LaunchId) -> Option<ManagedModel> {
    self.inner.write().await.remove(id)
  }

  /// Snapshot of every (LaunchId, ManagedModel) pair, sorted by id.
  pub async fn snapshot(&self) -> Vec<(LaunchId, ManagedModel)> {
    self
      .inner
      .read()
      .await
      .iter()
      .map(|(k, v)| (k.clone(), v.clone()))
      .collect()
  }

  pub async fn len(&self) -> usize {
    self.inner.read().await.len()
  }

  pub async fn is_empty(&self) -> bool {
    self.inner.read().await.is_empty()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn next_id_is_monotonic_within_one_registry() {
    let r = SupervisorRegistry::new();
    let a = r.next_id();
    let b = r.next_id();
    assert_ne!(a, b);
    // Two registries are independent.
    let other = SupervisorRegistry::new();
    let c = other.next_id();
    assert_eq!(c.as_str(), "L1");
  }

  #[test]
  fn launch_id_round_trips_via_json() {
    let id = LaunchId::from_counter(42);
    let s = serde_json::to_string(&id).unwrap();
    assert_eq!(s, "\"L42\"");
    let back: LaunchId = serde_json::from_str(&s).unwrap();
    assert_eq!(back, id);
  }
}
