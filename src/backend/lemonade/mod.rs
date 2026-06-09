//! Lemonade (`lemond`) managed-multiplexer backend.
//!
//! `lemond` is a long-lived umbrella process exposing an OpenAI-compatible
//! HTTP API; llamastash supervises the one process and delegates per-model
//! start / list to its API (R10). This module holds the typed API
//! [`client`], the [`Backend`](crate::backend::Backend) implementation
//! ([`backend`]), and the umbrella lifecycle ([`orchestrate`]).

pub mod backend;
pub mod client;
pub mod orchestrate;

pub use backend::{LemonadeBackend, LEMONADE_BACKEND_ID};
pub use client::{LemonadeClient, LemonadeError};
pub use orchestrate::{ensure_umbrella, umbrella_launch_id};
