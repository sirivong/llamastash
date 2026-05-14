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
pub async fn dispatch(cli: Cli, _config: LoadedConfig) -> Result<()> {
  match cli.command {
    None => unimplemented!("TUI entry — Unit 6"),
    Some(Command::Daemon(action)) => daemon::handle(action).await,
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
