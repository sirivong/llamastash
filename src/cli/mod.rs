//! CLI surface (clap definitions + dispatcher).
//!
//! The dispatcher is `async` because Unit 2's daemon client speaks Tokio.
//! Subcommands not yet implemented remain as `unimplemented!` so callers
//! see the wiring is in place without claiming work that isn't done.

pub mod cli_args;
pub mod daemon;

use anyhow::Result;

use crate::config::loader::LoadedConfig;

// Public surface kept ready for later units (TUI, supervisor, CLI handlers).
// Quiet the dead-code re-export warning until those consumers land.
#[allow(unused_imports)]
pub use cli_args::{
  Cli, Command, DaemonAction, FavoritesAction, LaunchMode, PresetsAction, PullAction, ReasoningFlag,
};

/// Dispatch the parsed CLI to its handler. The `config` argument carries
/// the merged user-config (loaded with the `--config` override already
/// applied) so handlers don't have to re-resolve the file path.
pub async fn dispatch(mut cli: Cli, config: LoadedConfig) -> Result<()> {
  // The `daemon` handler resolves scan roots from global flags + the
  // loaded config; other handlers don't (yet) need either. Keep both
  // bound here so future handlers can pick them up without re-shaping
  // the dispatcher.
  if let Some(warning) = &config.warning {
    log::warn!("{warning}");
  }
  // Splitting `command` off lets later arms still borrow `cli` for its
  // global flags (`model_paths`, `no_scan`) without fighting partial-
  // move rules around the per-subcommand owned data.
  let command = cli.command.take();
  let resolved_config = &config.config;
  match command {
    None => handle_tui(&cli, resolved_config).await,
    Some(Command::Daemon(action)) => daemon::handle(action, &cli, resolved_config).await,
    Some(Command::List(_)) => unimplemented!("list — Unit 8"),
    Some(Command::Start(_)) => unimplemented!("start — Unit 8"),
    Some(Command::Stop(_)) => unimplemented!("stop — Unit 8"),
    Some(Command::Status(_)) => unimplemented!("status — Unit 8"),
    Some(Command::Logs(_)) => unimplemented!("logs — Unit 8"),
    Some(Command::Presets(_)) => unimplemented!("presets — Unit 8"),
    // TODO(v2-R46): wire the HF pull worker. v1 scope was reduced
    // mid-Unit-4; the subcommand surface stays scaffolded but the
    // dispatcher must never claim work that isn't done.
    Some(Command::Pull(_)) => unimplemented!("pull — deferred to v2 (R46)"),
    Some(Command::Favorites(_)) => unimplemented!("favorites — Unit 5 / 8"),
  }
}

/// Entry point for the TUI (`llamatui` with no subcommand).
///
/// Resolves the daemon socket path the same way `daemon stop`
/// does, then hands off to [`crate::tui::events::launch`]. The
/// TUI's run-loop owns the alternate-screen lifecycle; this
/// function returns once the user quits.
async fn handle_tui(_cli: &Cli, config: &crate::config::Config) -> Result<()> {
  let socket = crate::util::paths::runtime_socket_path();
  crate::tui::events::launch(config.theme, &socket).await
}
