#![warn(rust_2018_idioms)]
#![deny(clippy::shadow_unrelated)]
// Unit 1 lands the scaffold: configs, themes, CLI surface, and path helpers
// are wired in but their consumers (daemon, supervisor, TUI, scanner) land in
// later units. Allow dead code crate-wide while the scaffold is incomplete;
// remove this allow once Unit 2+ start consuming these surfaces.
#![allow(dead_code)]

mod banner;
mod cli;
mod config;
mod theme;
mod util;

use anyhow::Result;
use clap::Parser;

use crate::cli::Cli;
use crate::config::loader;
use crate::util::logging;

#[tokio::main]
async fn main() -> Result<()> {
  let cli = Cli::parse();

  // Logger must be initialised BEFORE the panic hook — `log::error!` inside
  // the hook is a silent no-op while no logger is registered, so a panic
  // during CLI parsing/early startup would otherwise leave no trace in the
  // log file. Both calls are best-effort: a missing log dir or an already
  // initialised logger shouldn't block CLI use.
  let _ = logging::init(cli.verbose);
  logging::install_panic_hook();

  let config = loader::load_config(cli.config.clone());
  cli::dispatch(cli, config)
}
