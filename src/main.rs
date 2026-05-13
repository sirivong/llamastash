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
use crate::util::logging;

#[tokio::main]
async fn main() -> Result<()> {
  logging::install_panic_hook();

  let cli = Cli::parse();
  let _ = logging::init(cli.verbose); // best-effort; missing log dir shouldn't block CLI use

  cli::dispatch(cli)
}
