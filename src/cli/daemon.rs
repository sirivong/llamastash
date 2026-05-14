//! CLI handlers for the `daemon` subcommand.
//!
//! `start [--detach]` — launch the daemon. Foreground holds the
//! terminal; `--detach` returns once the socket is bound.
//! `stop` — connect to the daemon and call `shutdown`.
//! `status` — connect to the daemon and report PID + uptime; emits "not
//! running" if the socket is missing or the connection fails.

use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result};

use crate::cli::cli_args::DaemonAction;
use crate::daemon::{run_foreground, start_detached, DaemonOptions, StartOutcome};
use crate::ipc::{Client, ClientError};
use crate::util::paths::runtime_socket_path;

/// Top-level dispatch for `daemon <action>`.
pub async fn handle(action: DaemonAction) -> Result<()> {
  match action {
    DaemonAction::Start {
      detach,
      state_dir,
      socket_path,
    } => handle_start(detach, state_dir, socket_path).await,
    DaemonAction::Stop => handle_stop().await,
    DaemonAction::Status => handle_status().await,
  }
}

async fn handle_start(
  detach: bool,
  state_dir: Option<PathBuf>,
  socket_path: Option<PathBuf>,
) -> Result<()> {
  let opts = build_options(state_dir, socket_path)?;
  if detach {
    // `start_detached` blocks until the child reports socket bound.
    match start_detached(opts)? {
      StartOutcome::RanToCompletion => {
        println!("daemon: started (detached)");
        Ok(())
      }
      StartOutcome::AlreadyRunning(pid) => {
        println!("daemon: already running (pid {pid})");
        Ok(())
      }
    }
  } else {
    match run_foreground(opts).await? {
      StartOutcome::RanToCompletion => Ok(()),
      StartOutcome::AlreadyRunning(pid) => {
        println!("daemon: already running (pid {pid})");
        Ok(())
      }
    }
  }
}

async fn handle_stop() -> Result<()> {
  let socket = runtime_socket_path();
  match Client::connect(&socket).await {
    Ok(mut client) => {
      let _ = client.call("shutdown", None).await?;
      println!("daemon: shutdown requested");
      Ok(())
    }
    Err(ClientError::Connect(_)) => {
      println!("daemon: not running");
      Ok(())
    }
    Err(other) => Err(other).context("daemon stop"),
  }
}

/// Compose [`DaemonOptions`] from the parsed CLI overrides. Hidden
/// `--state-dir` / `--socket-path` flags take precedence; unset fields
/// fall back to the platform-default XDG paths. Centralised so the
/// re-exec'd child of `start_detached` honours the same priority order
/// as a hand-typed `llamatui daemon start --state-dir ...` invocation.
fn build_options(
  state_dir: Option<PathBuf>,
  socket_path: Option<PathBuf>,
) -> Result<DaemonOptions> {
  let mut opts = DaemonOptions::from_defaults()?;
  if let Some(p) = state_dir {
    opts.state_dir = p;
  }
  if let Some(p) = socket_path {
    opts.socket_path = p;
  }
  Ok(opts)
}

async fn handle_status() -> Result<()> {
  let socket = runtime_socket_path();
  // Short timeout for status — agents shouldn't sit on a dead socket.
  match Client::connect(&socket).await {
    Ok(mut client) => {
      let result = client
        .call_with_timeout("version", None, Duration::from_secs(2))
        .await?;
      println!("{}", serde_json::to_string_pretty(&result)?);
      Ok(())
    }
    Err(ClientError::Connect(_)) => {
      println!("daemon: not running");
      Ok(())
    }
    Err(other) => Err(other).context("daemon status"),
  }
}
