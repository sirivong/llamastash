//! Lemonade (`lemond`) managed-multiplexer backend.
//!
//! `lemond` is a long-lived umbrella process exposing an OpenAI-compatible
//! HTTP API; llamastash supervises the one process and delegates per-model
//! start / list to its API. This module holds the typed API
//! [`client`], the [`Backend`](crate::backend::Backend) implementation
//! ([`backend`]), and the umbrella lifecycle ([`orchestrate`]).

pub mod backend;
pub mod client;
pub mod orchestrate;

pub use backend::{
  registry_name_from_path, resolve_lemond_binary, umbrella_port_available, umbrella_port_state,
  umbrella_process_spec, LemonadeBackend, UmbrellaPortState, LEMONADE_BACKEND_ID,
  LEMONADE_PATH_SCHEME,
};
pub use client::{LemonadeClient, LemonadeError, LoadOptions, ModelEntry};
pub use orchestrate::{ensure_umbrella, umbrella_launch_id};
