//! Launch surface: everything the supervisor (Unit 5) needs to spawn
//! and parameterise a `llama-server` child.
//!
//! - [`binary`] — locate the `llama-server` executable on disk.
//! - [`params`] — compose the argv vector from user choices.
//! - [`mode`] — `LaunchMode` (chat/embedding/rerank) and helpers.
//! - [`presets`] / [`favorites`] — types persisted in
//!   [`crate::daemon::state_store`].

pub mod binary;
pub mod defaults_table;
pub mod favorites;
pub mod flag_aliases;
pub mod mode;
pub mod params;
pub mod presets;

pub use binary::{locate as locate_binary, LocateError, LocateInputs};
pub use defaults_table::lookup as lookup_defaults;
pub use favorites::{FavoriteEntry, Favorites};
pub use mode::LaunchMode;
pub use params::{compose, resolve_layered, LaunchParams, LayerLabel, Resolved};
pub use presets::{NamedPreset, Presets};
