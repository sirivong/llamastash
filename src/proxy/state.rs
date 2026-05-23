//! Shared state every proxy connection's handlers read from.
//!
//! Cloned-and-`Arc`-wrapped fields mirror the relevant slots of
//! [`crate::ipc::methods::MethodContext`] — the catalog, the
//! supervisor registry, persisted state, and the launch env — so
//! the proxy can answer requests without round-tripping through the
//! IPC dispatcher. Unit 1 only consumes `catalog` and `supervisors`
//! (for `/health`'s `models_loaded` / `models_discovered` counts);
//! later units lean on the rest.

use std::sync::Arc;
use std::time::Duration;

use crate::ipc::methods::MethodContext;

use super::coalesce::Coalesce;
use super::mru::MruTracker;

/// Cheap-to-clone bundle of the daemon-side handles the proxy needs.
/// The inner `Arc`s make per-connection cloning a single refcount
/// bump — the `service_fn` closure clones a fresh handle for every
/// inbound HTTP connection so handler futures don't borrow across
/// scheduler boundaries.
///
/// Catalog / supervisor registry / persisted state / launch env are
/// reached through [`ProxyState::ctx`] (the IPC `MethodContext`) so
/// there is a single source for daemon-side handles — no duplication
/// between flattened fields and `ctx`, which would let readers drift
/// out of sync with each other.
#[derive(Clone)]
pub struct ProxyState {
  /// Pooled HTTP client used by the forwarding path. One per proxy
  /// process — hyper handles keep-alive per-host inside the pool, so
  /// a second client would just be cargo cult. Wrapped in `Arc` so
  /// the per-connection clone is a refcount bump rather than
  /// rebuilding the pool.
  pub http_client: Arc<reqwest::Client>,
  /// Single-flight coalesce map for the auto-start path. Keyed on
  /// [`crate::gguf::identity::ModelId`] so two concurrent requests
  /// with different fuzzy spellings of the same model share one
  /// launch.
  pub(crate) coalesce: Coalesce,
  /// In-memory `last_request_at` tracker. The fallback selector
  /// reads from it; `route::forward_request` writes to it as
  /// forwarding starts (per the plan's "as it starts forwarding,
  /// not on completion" rule).
  pub(crate) mru: MruTracker,
  /// Full IPC context handle. Cheap to clone (every field is
  /// already `Arc`-wrapped); the proxy reads catalog / supervisors /
  /// persisted state / launch env through it so there is no
  /// duplicate-state risk.
  pub(crate) ctx: MethodContext,
}

impl ProxyState {
  /// Project the relevant handles out of an existing [`MethodContext`].
  /// The proxy task receives this handle from `run_foreground` after
  /// the rest of the daemon context has been assembled.
  pub fn from_context(ctx: &MethodContext) -> Arc<Self> {
    Arc::new(Self {
      http_client: Arc::new(build_http_client()),
      coalesce: Coalesce::new(),
      mru: MruTracker::new(),
      ctx: ctx.clone(),
    })
  }
}

/// Build the proxy's pooled HTTP client. Single source so tests can
/// reach into the same pool if they ever need to. The settings here
/// target the loopback `llama-server` upstream: short-ish connect
/// timeout (the child is on the same machine, anything > 5 s is a
/// real bug), no request timeout (chat completions are arbitrarily
/// long-running by design), pooling kept on so repeated requests
/// against the same port reuse keep-alive.
fn build_http_client() -> reqwest::Client {
  reqwest::Client::builder()
    .connect_timeout(Duration::from_secs(5))
    .pool_idle_timeout(Duration::from_secs(90))
    .build()
    // Builder failures here would be a misconfigured TLS stack /
    // missing certificate root. Loopback HTTP has none of those — we
    // never hit a network. If this ever panics in production the
    // build is broken, not the runtime.
    .expect("reqwest client must build on a healthy runtime")
}
