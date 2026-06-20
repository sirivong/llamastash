//! Inter-process communication between llamastash frontends (TUI, CLI) and
//! the daemon. JSON-RPC 2.0 over HTTP loopback with bearer-token auth.
//!
//! Module layout:
//! - `protocol` — JSON-RPC types (`Request`, `Response`, `ErrorObject`).
//! - `methods` — server-side method dispatch.
//! - `status` — assembly of the `status` method's response document.
//! - `client` — async client used by the TUI and CLI; talks the HTTP
//!   control plane defined in [`crate::daemon::control_plane`].

pub mod client;
pub mod methods;
pub mod protocol;
pub mod status;

pub use client::{Client, ClientError, DEFAULT_CALL_TIMEOUT};
pub use methods::dispatch_request;
pub use protocol::{ErrorCode, ErrorObject, Request, Response, JSONRPC_VERSION};
