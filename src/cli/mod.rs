//! CLI surface (clap definitions + dispatcher).
//!
//! Every subcommand is a stub in Unit 1; concrete handlers land alongside
//! the unit that owns the underlying behaviour. The dispatcher is sync
//! because Unit 1 has no async work — Unit 2's daemon client will switch
//! it to async via an internal helper, with `main` already on the tokio
//! runtime.

pub mod cli_args;

use anyhow::Result;

// Public surface kept ready for later units (TUI, supervisor, CLI handlers).
// Quiet the dead-code re-export warning until those consumers land.
#[allow(unused_imports)]
pub use cli_args::{Cli, Command, DaemonAction, LaunchMode, PresetsAction, ReasoningFlag};

/// Dispatch the parsed CLI to its handler. Unit 1 leaves the actual
/// per-command behaviour as `unimplemented!` placeholders so callers can
/// see the wiring is in place — `cargo build` succeeds and the help text
/// is complete — without claiming work that isn't done yet.
pub fn dispatch(cli: Cli) -> Result<()> {
  match cli.command {
    None => unimplemented!("TUI entry — Unit 6"),
    Some(Command::Daemon(DaemonAction::Start { .. })) => {
      unimplemented!("daemon start — Unit 2")
    }
    Some(Command::Daemon(DaemonAction::Stop)) => unimplemented!("daemon stop — Unit 2"),
    Some(Command::Daemon(DaemonAction::Status)) => unimplemented!("daemon status — Unit 2"),
    Some(Command::List(_)) => unimplemented!("list — Unit 8"),
    Some(Command::Start(_)) => unimplemented!("start — Unit 8"),
    Some(Command::Stop(_)) => unimplemented!("stop — Unit 8"),
    Some(Command::Status(_)) => unimplemented!("status — Unit 8"),
    Some(Command::Logs(_)) => unimplemented!("logs — Unit 8"),
    Some(Command::Presets(_)) => unimplemented!("presets — Unit 8"),
    Some(Command::Pull(_)) => unimplemented!("pull — Unit 4 / 8"),
  }
}
