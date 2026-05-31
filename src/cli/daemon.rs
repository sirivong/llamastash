//! CLI handlers for the `daemon` subcommand.
//!
//! `start [--foreground]` — launch the daemon. Default is detached: a
//! one-line "starting in background" notice prints, the parent re-execs
//! a child, waits for the socket to bind, and returns control to the
//! shell. `--foreground` (`-f`) keeps the daemon attached to the
//! controlling terminal for `systemd` / supervisor wrappers that own
//! stdout/stderr. The historical complaint was that bare `daemon start`
//! ran in the foreground and looked stuck.
//! `stop` — connect to the daemon and call `shutdown`.
//! `status` — connect to the daemon and report PID + uptime; emits "not
//! running" if the socket is missing or the connection fails.

use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result};

use crate::cli::cli_args::{Cli, DaemonAction};
use crate::config::Config;
use crate::daemon::discovery_task::DiscoveryOptions;
use crate::daemon::{
  existing_daemon_pid, run_foreground, start_detached, DaemonOptions, StartOutcome,
};
use crate::discovery::known_caches::{default_set, RootResolution};
use crate::ipc::{Client, ClientError};
use crate::launch::binary::{locate as locate_binary, LocateInputs};
use crate::util::paths::{home_dir, state_dir};

/// Top-level dispatch for `daemon <action>`. The full `Cli` and merged
/// `Config` flow through so `handle_start` can resolve discovery roots
/// from user flags + config; status / stop ignore them.
pub async fn handle(action: DaemonAction, cli: &Cli, config: &Config) -> Result<()> {
  match action {
    DaemonAction::Start {
      foreground,
      state_dir,
      proxy_port,
      ollama_compat,
      no_proxy_fallback,
    } => {
      handle_start(
        foreground,
        state_dir,
        proxy_port,
        ollama_compat,
        no_proxy_fallback,
        cli,
        config,
      )
      .await
    }
    DaemonAction::Stop { force } => handle_stop(force).await,
    DaemonAction::Status { json } => handle_status(json).await,
  }
}

// `handle_start` is the single thin shim that unpacks every
// `daemon start` flag and feeds them into `build_options`. Each new
// CLI flag added here costs an argument; the alternative (a typed
// `StartFlags` struct) would just push the unpack one level out
// without changing how much information crosses the boundary. Allow
// the count to grow with the CLI surface instead.
#[allow(clippy::too_many_arguments)]
async fn handle_start(
  foreground: bool,
  state_dir: Option<PathBuf>,
  proxy_port: Option<u16>,
  ollama_compat: bool,
  no_proxy_fallback: bool,
  cli: &Cli,
  config: &Config,
) -> Result<()> {
  let opts = build_options(
    state_dir,
    proxy_port,
    ollama_compat,
    no_proxy_fallback,
    cli,
    config,
  )?;
  if foreground {
    // `--foreground` (or `-f`) keeps the daemon attached to the
    // controlling terminal. Print a one-line notice up front so the
    // user sees *why* the prompt isn't coming back — otherwise the
    // silent hand-off makes a fresh `daemon start --foreground` look
    // stuck. Suppressed when an existing daemon already owns the
    // lockfile (we'd flash the notice then immediately fall through
    // to "already running", which would be confusing).
    if existing_daemon_pid(&opts.state_dir).is_none() {
      println!(
        "{}",
        crate::cli::colors::dim(
          "daemon: running in foreground — Ctrl+C to stop, or omit -f to background it",
        )
      );
    }
    match run_foreground(opts).await? {
      StartOutcome::RanToCompletion => Ok(()),
      StartOutcome::AlreadyRunning(pid) => {
        print_already_running(pid);
        Ok(())
      }
    }
  } else {
    // Default: detach into the background. The hand-off looks like
    // a hang otherwise (the parent waits for the child to bind its
    // socket — usually <100 ms, but a slow disk can stretch it), so
    // print a "starting" notice first and a green-check confirmation
    // once `start_detached` returns. Suppressed when an existing
    // daemon already owns the lockfile (skips straight to the
    // "already running" line below).
    if existing_daemon_pid(&opts.state_dir).is_none() {
      println!(
        "{}",
        crate::cli::colors::dim("daemon: starting in background…")
      );
    }
    // `start_detached` blocks until the child reports socket bound.
    match start_detached(opts)? {
      StartOutcome::RanToCompletion => {
        println!(
          "{}",
          crate::cli::colors::success("daemon: started (detached)")
        );
        Ok(())
      }
      StartOutcome::AlreadyRunning(pid) => {
        print_already_running(pid);
        Ok(())
      }
    }
  }
}

/// "daemon: already running (pid N)" — emitted from both the `--detach`
/// and the foreground branches of `handle_start` when the daemon is
/// already up. "daemon: already running" stays dim; the pid lifts to
/// bold so it's the scannable token in the otherwise dim line.
fn print_already_running(pid: i32) {
  println!(
    "{} ({} {})",
    crate::cli::colors::dim("daemon: already running"),
    crate::cli::colors::dim("pid"),
    console::style(pid.to_string()).bold(),
  );
}

async fn handle_stop(force: bool) -> Result<()> {
  let attach_dir = state_dir().context("could not resolve state directory")?;
  if !force {
    match Client::connect(&attach_dir).await {
      Ok(mut client) => {
        let _ = client.call("shutdown", None).await?;
        println!(
          "{}",
          crate::cli::colors::success("daemon: shutdown requested")
        );
        return Ok(());
      }
      // No IPC channel — either the daemon is genuinely down, or it's
      // a stale process that didn't publish runtime.json. The
      // `existing_daemon_pid` check below distinguishes the two.
      Err(ClientError::Connect(_)) => {}
      Err(other) => return Err(other).context("daemon stop"),
    }
  }
  match existing_daemon_pid(&attach_dir) {
    None => {
      println!("{}", crate::cli::colors::dim("daemon: not running"));
      Ok(())
    }
    Some(pid) => force_stop_via_pid(pid, &attach_dir),
  }
}

/// Best-effort PID-based shutdown. Used when the IPC channel is
/// unusable (no `runtime.json`) or when the user passed `--force`.
/// Sends `SIGTERM` via [`ProcessControl`], waits up to ~3s for the
/// lockfile to release, then surfaces a clear next-step (`SIGKILL`)
/// if the daemon ignores the signal.
fn force_stop_via_pid(pid: i32, attach_dir: &std::path::Path) -> Result<()> {
  use crate::util::process_control::{platform_default, SignalTarget};
  use std::time::{Duration, Instant};
  if pid <= 0 {
    return Err(anyhow::anyhow!(
      "daemon stop --force: invalid pid {pid} in lockfile"
    ));
  }
  let pc = platform_default();
  let pid_u = pid as u32;
  // Pre-check via `is_alive` so we can surface "already exited" without
  // having to inspect signal-syscall errno. The trait swallows ESRCH
  // internally, which is the right behavior for the supervisor but
  // would hide the user-relevant distinction here.
  if !pc.is_alive(pid_u) {
    println!(
      "{}",
      crate::cli::colors::dim(&format!("daemon: pid {pid} already exited"))
    );
    return Ok(());
  }
  pc.signal_graceful(SignalTarget::SinglePid(pid_u));
  let deadline = Instant::now() + Duration::from_secs(3);
  while Instant::now() < deadline {
    if existing_daemon_pid(attach_dir).is_none() {
      println!(
        "{}",
        crate::cli::colors::success(&format!("daemon: stopped (pid {pid})"))
      );
      return Ok(());
    }
    std::thread::sleep(Duration::from_millis(50));
  }
  Err(anyhow::anyhow!(
    "daemon stop --force: pid {pid} did not exit within 3s after SIGTERM; \
     try `kill -KILL {pid}` and check the daemon logs"
  ))
}

/// Compose [`DaemonOptions`] from the parsed CLI overrides. Hidden
/// `--state-dir` / `--socket-path` flags take precedence; unset fields
/// fall back to the platform-default XDG paths. Centralised so the
/// re-exec'd child of `start_detached` honours the same priority order
/// as a hand-typed `llamastash daemon start --state-dir ...` invocation.
///
/// Discovery roots are resolved here so a real `llamastash daemon start`
/// (not just tests) populates the catalog: global `--model-path` /
/// `--no-scan` / `--config` plus `config.model_paths` /
/// `config.disable_default_cache_paths` / `config.disable_scan` feed
/// `known_caches::default_set`. An empty config + no flags still
/// produces a working daemon — the daemon just operates with whichever
/// HF/Ollama/LM Studio caches exist on disk.
pub(crate) fn build_options(
  state_dir: Option<PathBuf>,
  proxy_port: Option<u16>,
  ollama_compat_cli: bool,
  no_proxy_fallback_cli: bool,
  cli: &Cli,
  config: &Config,
) -> Result<DaemonOptions> {
  let env_paths = env_model_paths();
  let env_no_scan_v = env_no_scan();
  // Refuse to start with discovery completely off and no user-supplied
  // paths. Without this the daemon would come up healthy, the catalog
  // would stay empty forever, and the user would see "no models found"
  // with no signal that it's a config dead-end. Errors propagate as
  // CONFIG_ERROR exits via the CLI dispatcher.
  crate::config::validate_scan_settings(
    cli.no_scan || env_no_scan_v || config.disable_scan,
    &cli.model_paths,
    &env_paths,
    &config.model_paths,
  )
  .map_err(anyhow::Error::from)?;

  let mut opts = DaemonOptions::from_defaults()?;
  if let Some(p) = state_dir {
    opts.state_dir = p;
  }
  let scan_roots = resolve_scan_roots(
    cli,
    config,
    &env_paths,
    env_no_scan_v,
    home_dir().as_deref(),
  );
  opts.discovery = DiscoveryOptions::new(scan_roots);
  // Best-effort `llama-server` resolution. A miss leaves
  // `opts.binary = None`; the daemon still starts and `start_model`
  // surfaces an actionable error to the caller. We log so the user
  // sees *why* a later launch fails.
  opts.binary = match locate_binary(LocateInputs {
    cli_flag: cli.llama_server.clone(),
    env_var: std::env::var_os("LLAMASTASH_LLAMA_SERVER"),
    config_path: config.llama_server_path.clone(),
  }) {
    Ok(p) => Some(p),
    Err(e) => {
      log::warn!("llama-server lookup failed: {e}");
      None
    }
  };
  opts.port_range = config.port_range;
  opts.probe_timeout_secs = Some(config.probe_timeout_secs);
  opts.arch_defaults = config.arch_defaults.clone();
  // Proxy: config layer first, then CLI / env overrides. Without this
  // thread-through the daemon silently ignored `proxy:` from the config
  // file and ran with `ProxyConfig::default()` regardless.
  opts.proxy = config.proxy.clone();
  if let Some(p) = proxy_port {
    opts.proxy.port = Some(p);
  }
  // Ollama-compat: OR of (config field, `--ollama-compat` CLI flag,
  // `LLAMASTASH_OLLAMA_COMPAT` env var). Any one of the three enables
  // the mode — clearing it requires unsetting all three. The env var
  // accepts the usual truthy strings; anything else (incl. unset) is
  // treated as false.
  let env_compat = env_flag_truthy("LLAMASTASH_OLLAMA_COMPAT");
  opts.proxy.ollama_compat = opts.proxy.ollama_compat || ollama_compat_cli || env_compat;
  // Fallback-disable: OR of (config field cleared, `--no-proxy-fallback`
  // CLI flag, `LLAMASTASH_NO_PROXY_FALLBACK` env var). Any one of the
  // three turns the family-MRU fallback off — re-enabling requires
  // unsetting all of them and keeping `fallback_enabled: true` in the
  // config (which is the default). The CLI flag can only disable; we
  // never re-enable from CLI on top of a config-level `false`.
  let env_no_fallback = env_flag_truthy("LLAMASTASH_NO_PROXY_FALLBACK");
  if no_proxy_fallback_cli || env_no_fallback {
    opts.proxy.fallback_enabled = false;
  }
  opts.propagated_cli_args = propagated_cli_args(cli);
  Ok(opts)
}

/// True when the named env var is set to a recognised truthy string.
/// Accepts the usual flavours (`1`, `true`, `yes`, `on`) regardless of
/// case; empty / unset / anything else is false. Trimmed before
/// matching so a stray newline (common in `.env` files) doesn't bite.
fn env_flag_truthy(name: &str) -> bool {
  std::env::var(name)
    .ok()
    .map(|v| {
      matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
      )
    })
    .unwrap_or(false)
}

/// Parse `LLAMASTASH_MODEL_PATHS` into a path list, using the
/// platform's path separator (`:` on POSIX, `;` on Windows) via
/// `std::env::split_paths`. Unset / empty / all-empty-entries returns
/// an empty vec. Empty entries (`""` between separators) are skipped
/// rather than producing `PathBuf::new()` — a stray colon shouldn't
/// register as "scan the empty path".
fn env_model_paths() -> Vec<PathBuf> {
  std::env::var_os("LLAMASTASH_MODEL_PATHS")
    .map(|raw| {
      std::env::split_paths(&raw)
        .filter(|p| !p.as_os_str().is_empty())
        .collect()
    })
    .unwrap_or_default()
}

/// True when `LLAMASTASH_NO_SCAN` is set to a truthy value. Same
/// truthy set as `env_flag_truthy` — matches the documented
/// `LLAMASTASH_NO_SCAN=1` recipe in the README, and accepts
/// `true`/`yes`/`on` (case-insensitive) for parity with other env
/// flags in this binary.
fn env_no_scan() -> bool {
  env_flag_truthy("LLAMASTASH_NO_SCAN")
}

/// Collect the global CLI flags that the re-exec'd detached daemon
/// must inherit so it resolves the same discovery / binary / config
/// surface the parent would have. Without this, the child rebuilds
/// `DaemonOptions` from an empty `Cli` and the user's `-p
/// /some/path`, `--no-scan`, `--llama-server`, `--config` flags are
/// silently dropped.
fn propagated_cli_args(cli: &Cli) -> Vec<std::ffi::OsString> {
  let mut args: Vec<std::ffi::OsString> = Vec::new();
  for path in &cli.model_paths {
    args.push("--model-path".into());
    args.push(path.into());
  }
  if cli.no_scan {
    args.push("--no-scan".into());
  }
  if let Some(p) = &cli.llama_server {
    args.push("--llama-server".into());
    args.push(p.into());
  }
  if let Some(p) = &cli.config {
    args.push("--config".into());
    args.push(p.into());
  }
  args
}

/// Merge CLI + env + config + default-cache enumeration into the
/// canonical scan-root list the daemon should walk. Exposed as a pure
/// function so unit tests can drive it without touching process env.
///
/// Priority for `no_scan`: any of (`--no-scan`,
/// `LLAMASTASH_NO_SCAN=1`, `disable_scan: true`) turns the default-
/// cache walk off. Model paths from all three sources are merged and
/// de-duplicated; merge order is `config > env > cli` (irrelevant for
/// correctness since the list is order-insensitive at the scanner,
/// but it keeps the daemon's `ps`-visible argv readable).
pub(crate) fn resolve_scan_roots(
  cli: &Cli,
  config: &Config,
  env_paths: &[PathBuf],
  env_no_scan: bool,
  home: Option<&std::path::Path>,
) -> Vec<crate::discovery::scanner::ScanRoot> {
  let mut user_paths: Vec<PathBuf> = config.model_paths.clone();
  for p in env_paths {
    if !user_paths.iter().any(|x| x == p) {
      user_paths.push(p.clone());
    }
  }
  for p in &cli.model_paths {
    if !user_paths.iter().any(|x| x == p) {
      user_paths.push(p.clone());
    }
  }
  let no_scan = cli.no_scan || env_no_scan || config.disable_scan;
  default_set(RootResolution {
    user_paths: &user_paths,
    disable: &config.disable_default_cache_paths,
    no_scan,
    home,
  })
}

async fn handle_status(json: bool) -> Result<()> {
  let attach_dir = state_dir().context("could not resolve state directory")?;
  // Short timeout for status — agents shouldn't sit on a dead daemon.
  match Client::connect(&attach_dir).await {
    Ok(mut client) => {
      let result = client
        .call_with_timeout("version", None, Duration::from_secs(2))
        .await?;
      if json {
        // Machine contract: the raw `version` IPC response, byte-stable
        // across releases. Agents that previously piped `daemon status`
        // to `jq` should pass `--json` to keep their parser working.
        println!(
          "{}",
          serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string())
        );
      } else {
        print!("{}", render_daemon_status(&result));
      }
      Ok(())
    }
    Err(ClientError::Connect(_)) => {
      // A live process owning `daemon.pid` but no `runtime.json` is a
      // stale daemon from an older binary (or a crash between
      // lock-acquire and runtime-file save). Report it distinctly so
      // an operator knows to run `daemon stop --force` instead of
      // assuming the daemon is genuinely down.
      let stale_pid = existing_daemon_pid(&attach_dir);
      if json {
        let envelope = match stale_pid {
          Some(pid) => serde_json::json!({"daemon": "stale", "pid": pid}),
          None => serde_json::json!({"daemon": "not_running"}),
        };
        println!("{envelope}");
      } else {
        match stale_pid {
          Some(pid) => {
            println!(
              "{}",
              crate::cli::colors::dim(&format!(
                "daemon: stale (pid {pid}) — run `llamastash daemon stop --force` to recover"
              ))
            );
          }
          None => {
            println!("{}", crate::cli::colors::dim("daemon: not running"));
          }
        }
      }
      Ok(())
    }
    Err(other) => Err(other).context("daemon status"),
  }
}

/// Render the daemon's `version` IPC response as a labelled key/value
/// block. Falls back to pretty-JSON when the response doesn't carry the
/// expected fields so we never lose info on an unrecognised schema.
///
/// Fields surfaced (from `version` IPC): `name`, `version`,
/// `protocol_version`, `pid`, `uptime_seconds`, `connections`. Missing
/// fields render as a dim `-`.
fn render_daemon_status(body: &serde_json::Value) -> String {
  use crate::cli::{colors, format};
  use serde_json::Value;

  let Some(obj) = body.as_object() else {
    // Unexpected shape: emit a dim warning and the raw pretty-JSON so
    // the user still sees what came back. Avoids silently swallowing
    // info on an out-of-band response.
    let mut out = colors::dim("daemon: unexpected version response shape; raw body follows");
    out.push('\n');
    out.push_str(&serde_json::to_string_pretty(body).unwrap_or_else(|_| body.to_string()));
    out.push('\n');
    return out;
  };

  let dim_dash = || colors::dim("-");
  let str_field = |key: &str| {
    obj
      .get(key)
      .and_then(Value::as_str)
      .map(str::to_string)
      .unwrap_or_else(dim_dash)
  };
  let u64_field = |key: &str| {
    obj
      .get(key)
      .and_then(Value::as_u64)
      .map(|n| n.to_string())
      .unwrap_or_else(dim_dash)
  };

  let pid = obj
    .get("pid")
    .and_then(Value::as_u64)
    .map(|n| console::style(n.to_string()).bold().to_string())
    .unwrap_or_else(dim_dash);
  let uptime = obj
    .get("uptime_seconds")
    .and_then(Value::as_u64)
    .map(format::format_uptime)
    .unwrap_or_else(dim_dash);

  let mut out = String::new();
  out.push_str(&format::section_header("daemon", None));
  out.push_str(&format::kv_block(&[
    ("name", str_field("name")),
    ("version", str_field("version")),
    ("protocol", u64_field("protocol_version")),
    ("pid", pid),
    ("uptime", uptime),
    ("connections", u64_field("connections")),
  ]));
  out
}

#[cfg(test)]
mod tests {
  use std::path::Path;

  use clap::Parser;

  use super::*;
  use crate::cli::cli_args::Cli;
  use crate::config::Config;
  use crate::discovery::ModelSource;

  fn parse_cli(args: &[&str]) -> Cli {
    Cli::try_parse_from(std::iter::once("llamastash").chain(args.iter().copied())).expect("parse")
  }

  #[test]
  fn resolve_uses_config_model_paths_and_cli_model_paths_together() {
    let cli = parse_cli(&["--model-path", "/work/cli-only", "daemon", "start"]);
    let config = Config {
      model_paths: vec![PathBuf::from("/work/cfg-only")],
      ..Config::default()
    };
    let home = PathBuf::from("/home/alice");
    let roots = resolve_scan_roots(&cli, &config, &[], false, Some(&home));
    let user_paths: Vec<&Path> = roots
      .iter()
      .filter(|r| r.source == ModelSource::UserPath)
      .map(|r| r.path.as_path())
      .collect();
    assert!(
      user_paths.iter().any(|p| *p == Path::new("/work/cfg-only")),
      "config path missing: {user_paths:?}"
    );
    assert!(
      user_paths.iter().any(|p| *p == Path::new("/work/cli-only")),
      "cli path missing: {user_paths:?}"
    );
  }

  #[test]
  fn no_scan_flag_suppresses_default_caches() {
    let cli = parse_cli(&["--no-scan", "--model-path", "/work/keep", "daemon", "start"]);
    let config = Config::default();
    let home = PathBuf::from("/home/alice");
    let roots = resolve_scan_roots(&cli, &config, &[], false, Some(&home));
    let cache_sources: Vec<_> = roots
      .iter()
      .filter(|r| r.source != ModelSource::UserPath)
      .map(|r| r.source)
      .collect();
    assert!(
      cache_sources.is_empty(),
      "--no-scan must suppress default caches, got {cache_sources:?}"
    );
    assert_eq!(roots.len(), 1, "only --model-path remains");
  }

  #[test]
  fn config_disable_scan_also_suppresses_default_caches() {
    let cli = parse_cli(&["daemon", "start"]);
    let config = Config {
      disable_scan: true,
      ..Config::default()
    };
    let home = PathBuf::from("/home/alice");
    let roots = resolve_scan_roots(&cli, &config, &[], false, Some(&home));
    assert!(roots.is_empty(), "no user paths + disable_scan = empty");
  }

  #[test]
  fn config_disable_default_cache_paths_honoured() {
    let cli = parse_cli(&["daemon", "start"]);
    let config = Config {
      disable_default_cache_paths: crate::config::CachePathsConfig {
        huggingface: true,
        ollama: true,
        lm_studio: false,
      },
      ..Config::default()
    };
    let home = PathBuf::from("/home/alice");
    let roots = resolve_scan_roots(&cli, &config, &[], false, Some(&home));
    let sources: std::collections::BTreeSet<_> = roots.iter().map(|r| r.source).collect();
    assert!(!sources.contains(&ModelSource::HuggingFace));
    assert!(!sources.contains(&ModelSource::Ollama));
    // LM Studio default is still enabled.
    assert!(sources.contains(&ModelSource::LmStudio));
  }

  #[test]
  fn duplicate_paths_across_cli_and_config_collapse() {
    let cli = parse_cli(&["--model-path", "/shared", "daemon", "start"]);
    let config = Config {
      model_paths: vec![PathBuf::from("/shared")],
      ..Config::default()
    };
    let home = PathBuf::from("/home/alice");
    let roots = resolve_scan_roots(&cli, &config, &[], false, Some(&home));
    let matches: Vec<&Path> = roots
      .iter()
      .filter(|r| r.path == Path::new("/shared"))
      .map(|r| r.path.as_path())
      .collect();
    assert_eq!(matches.len(), 1, "duplicate must collapse, got {matches:?}");
  }

  #[test]
  fn propagated_cli_args_carry_model_paths_no_scan_and_overrides() {
    // Regression: `start_detached` previously dropped every global
    // flag on re-exec, so an auto-spawned daemon silently ignored
    // the user's `-p /some/path` and `--no-scan`. `build_options`
    // now stamps these onto `DaemonOptions::propagated_cli_args` so
    // the child sees the same CLI surface as the parent.
    let cli = parse_cli(&[
      "--model-path",
      "/work/a",
      "--model-path",
      "/work/b",
      "--no-scan",
      "--llama-server",
      "/usr/local/bin/llama-server",
      "--config",
      "/etc/llamastash.yaml",
      "daemon",
      "start",
    ]);
    let args = propagated_cli_args(&cli);
    let as_str: Vec<&str> = args.iter().map(|s| s.to_str().unwrap()).collect();
    assert_eq!(
      as_str,
      vec![
        "--model-path",
        "/work/a",
        "--model-path",
        "/work/b",
        "--no-scan",
        "--llama-server",
        "/usr/local/bin/llama-server",
        "--config",
        "/etc/llamastash.yaml",
      ]
    );
  }

  #[test]
  fn propagated_cli_args_is_empty_when_no_global_flags_set() {
    let cli = parse_cli(&["daemon", "start"]);
    assert!(propagated_cli_args(&cli).is_empty());
  }

  #[test]
  fn render_daemon_status_emits_labelled_fields_for_well_formed_response() {
    let body = serde_json::json!({
      "name": "llamastash",
      "version": "0.0.1",
      "protocol_version": 1,
      "pid": 4242,
      "uptime_seconds": 90,
      "connections": 3,
    });
    let rendered = render_daemon_status(&body);
    let plain = console::strip_ansi_codes(&rendered);
    // section header + 6 kv rows.
    assert!(
      plain.starts_with("daemon\n"),
      "missing section header: {plain:?}"
    );
    assert!(plain.contains("name  llamastash"));
    assert!(plain.contains("version  0.0.1"));
    assert!(plain.contains("protocol  1"));
    assert!(plain.contains("pid  4242"));
    assert!(plain.contains("uptime  1m 30s"));
    assert!(plain.contains("connections  3"));
  }

  #[test]
  fn render_daemon_status_renders_missing_fields_as_dim_dash() {
    let body = serde_json::json!({"name": "llamastash"});
    let rendered = render_daemon_status(&body);
    let plain = console::strip_ansi_codes(&rendered);
    // pid / uptime / etc. all fall back to the "-" sentinel.
    assert!(plain.contains("name  llamastash"));
    assert!(plain.contains("pid  -"));
    assert!(plain.contains("uptime  -"));
    assert!(plain.contains("protocol  -"));
    assert!(plain.contains("connections  -"));
  }

  #[test]
  fn render_daemon_status_falls_back_to_raw_json_for_non_object_body() {
    let body = serde_json::json!([1, 2, 3]);
    let rendered = render_daemon_status(&body);
    let plain = console::strip_ansi_codes(&rendered);
    assert!(plain.contains("unexpected version response shape"));
    assert!(plain.contains("[\n  1,"));
  }

  #[test]
  fn build_options_threads_config_proxy_block_into_daemon_options() {
    // Regression: before this wiring landed, config.proxy.port was
    // parsed and validated but `build_options` never copied it onto
    // DaemonOptions.proxy. The daemon silently ran with
    // ProxyConfig::default() no matter what the user put in
    // config.yaml.
    let cli = parse_cli(&["daemon", "start"]);
    let config = Config {
      proxy: crate::config::loader::ProxyConfig {
        enabled: true,
        port: Some(22222),
        ollama_compat: false,
        fallback_enabled: true,
        header_read_timeout_secs: 45,
        idle_ttl_secs: 1800,
      },
      ..Config::default()
    };
    let opts = build_options(None, None, false, false, &cli, &config).expect("build_options");
    assert_eq!(
      opts.proxy.port,
      Some(22222),
      "config proxy.port must reach daemon"
    );
    assert_eq!(opts.proxy.effective_port(), 22222);
    assert_eq!(opts.proxy.header_read_timeout_secs, 45);
    assert!(opts.proxy.enabled);
    assert!(!opts.proxy.ollama_compat);
  }

  #[test]
  fn build_options_proxy_port_cli_overrides_config_value() {
    let cli = parse_cli(&["daemon", "start"]);
    let config = Config {
      proxy: crate::config::loader::ProxyConfig {
        enabled: true,
        port: Some(22222),
        ollama_compat: false,
        fallback_enabled: true,
        header_read_timeout_secs: 30,
        idle_ttl_secs: 1800,
      },
      ..Config::default()
    };
    // The CLI override (Some(8080)) beats config.proxy.port.
    let opts = build_options(None, Some(8080), false, false, &cli, &config).expect("build_options");
    assert_eq!(opts.proxy.port, Some(8080), "CLI flag overrides config");
    assert_eq!(opts.proxy.effective_port(), 8080);
    // Other proxy fields still come from config (not reset).
    assert!(opts.proxy.enabled);
    assert_eq!(opts.proxy.header_read_timeout_secs, 30);
  }

  #[test]
  fn build_options_no_cli_override_falls_back_to_config_then_default() {
    // Defaults all the way down: no CLI override, no proxy block in
    // config → daemon uses ProxyConfig::default(), which resolves to
    // 11435 (default mode) when nothing pins `port` explicitly.
    let cli = parse_cli(&["daemon", "start"]);
    let config = Config::default();
    let opts = build_options(None, None, false, false, &cli, &config).expect("build_options");
    assert_eq!(opts.proxy.port, None);
    assert_eq!(opts.proxy.effective_port(), 11435);
    assert!(!opts.proxy.ollama_compat);
  }

  #[test]
  fn build_options_ollama_compat_cli_flag_flips_mode_and_default_port() {
    let cli = parse_cli(&["daemon", "start"]);
    let config = Config::default();
    let opts = build_options(None, None, true, false, &cli, &config).expect("build_options");
    assert!(opts.proxy.ollama_compat);
    // Port stays None at the schema level — the CLI flag drives the
    // mode bool, and `effective_port()` derives the runtime port.
    assert_eq!(opts.proxy.port, None);
    assert_eq!(opts.proxy.effective_port(), 11434);
  }

  #[test]
  fn build_options_ollama_compat_or_combines_config_cli_env() {
    // Config-only: config says compat=true, CLI flag off → enabled.
    let cli = parse_cli(&["daemon", "start"]);
    let config_compat = Config {
      proxy: crate::config::loader::ProxyConfig {
        ollama_compat: true,
        ..crate::config::loader::ProxyConfig::default()
      },
      ..Config::default()
    };
    let opts_config =
      build_options(None, None, false, false, &cli, &config_compat).expect("build_options");
    assert!(opts_config.proxy.ollama_compat);

    // CLI-only: config has compat=false, CLI flag on → enabled.
    let config_off = Config::default();
    let opts_cli =
      build_options(None, None, true, false, &cli, &config_off).expect("build_options");
    assert!(opts_cli.proxy.ollama_compat);

    // Both off (neither config nor CLI) → disabled.
    let opts_neither =
      build_options(None, None, false, false, &cli, &config_off).expect("build_options");
    assert!(!opts_neither.proxy.ollama_compat);
  }

  #[test]
  fn build_options_no_proxy_fallback_cli_flag_clears_fallback_enabled() {
    let cli = parse_cli(&["daemon", "start"]);
    let config = Config::default();
    // Default is fallback_enabled = true.
    let baseline =
      build_options(None, None, false, false, &cli, &config).expect("build_options baseline");
    assert!(baseline.proxy.fallback_enabled);
    // CLI flag forces it off.
    let opts =
      build_options(None, None, false, true, &cli, &config).expect("build_options no-fallback");
    assert!(!opts.proxy.fallback_enabled);
  }

  #[test]
  fn build_options_no_proxy_fallback_or_combines_config_cli() {
    // Config-only: config has fallback_enabled=false, CLI off → disabled.
    let cli = parse_cli(&["daemon", "start"]);
    let config_off_fallback = Config {
      proxy: crate::config::loader::ProxyConfig {
        fallback_enabled: false,
        ..crate::config::loader::ProxyConfig::default()
      },
      ..Config::default()
    };
    let opts_config =
      build_options(None, None, false, false, &cli, &config_off_fallback).expect("build_options");
    assert!(!opts_config.proxy.fallback_enabled);

    // CLI-only: config has fallback_enabled=true (default), CLI on → disabled.
    let config_default = Config::default();
    let opts_cli =
      build_options(None, None, false, true, &cli, &config_default).expect("build_options");
    assert!(!opts_cli.proxy.fallback_enabled);

    // Both off → fallback_enabled stays true (the default).
    let opts_neither =
      build_options(None, None, false, false, &cli, &config_default).expect("build_options");
    assert!(opts_neither.proxy.fallback_enabled);
  }

  #[test]
  fn resolve_uses_env_paths_when_supplied() {
    // No CLI / config paths — env-only source must still populate
    // UserPath roots so a `LLAMASTASH_MODEL_PATHS=/foo:/bar` recipe
    // matches the documented contract in the README.
    let cli = parse_cli(&["daemon", "start"]);
    let config = Config::default();
    let home = PathBuf::from("/home/alice");
    let env_paths = vec![PathBuf::from("/env/one"), PathBuf::from("/env/two")];
    let roots = resolve_scan_roots(&cli, &config, &env_paths, false, Some(&home));
    let user_paths: Vec<&Path> = roots
      .iter()
      .filter(|r| r.source == ModelSource::UserPath)
      .map(|r| r.path.as_path())
      .collect();
    assert!(
      user_paths.iter().any(|p| *p == Path::new("/env/one")),
      "env path missing: {user_paths:?}"
    );
    assert!(
      user_paths.iter().any(|p| *p == Path::new("/env/two")),
      "env path missing: {user_paths:?}"
    );
  }

  #[test]
  fn resolve_dedupes_env_paths_against_cli_and_config() {
    let cli = parse_cli(&["--model-path", "/shared", "daemon", "start"]);
    let config = Config {
      model_paths: vec![PathBuf::from("/shared")],
      ..Config::default()
    };
    let env_paths = vec![PathBuf::from("/shared")];
    let home = PathBuf::from("/home/alice");
    let roots = resolve_scan_roots(&cli, &config, &env_paths, false, Some(&home));
    let matches: Vec<&Path> = roots
      .iter()
      .filter(|r| r.path == Path::new("/shared"))
      .map(|r| r.path.as_path())
      .collect();
    assert_eq!(
      matches.len(),
      1,
      "config/env/cli duplicate must collapse, got {matches:?}"
    );
  }

  #[test]
  fn resolve_env_no_scan_suppresses_default_caches() {
    let cli = parse_cli(&["--model-path", "/work/keep", "daemon", "start"]);
    let config = Config::default();
    let home = PathBuf::from("/home/alice");
    // env_no_scan=true must drop the default-cache walk even when
    // neither `--no-scan` nor `config.disable_scan` is set.
    let roots = resolve_scan_roots(&cli, &config, &[], true, Some(&home));
    let cache_sources: Vec<_> = roots
      .iter()
      .filter(|r| r.source != ModelSource::UserPath)
      .map(|r| r.source)
      .collect();
    assert!(
      cache_sources.is_empty(),
      "LLAMASTASH_NO_SCAN must suppress default caches, got {cache_sources:?}"
    );
    assert_eq!(roots.len(), 1, "only --model-path remains");
  }

  #[test]
  fn env_model_paths_splits_on_platform_separator() {
    // Drive the production helper directly. Two paths joined with the
    // platform separator must round-trip. `join_paths` is the inverse
    // of `split_paths`, so this also documents the public contract
    // shell users see (`:` on POSIX, `;` on Windows).
    let joined = std::env::join_paths([PathBuf::from("/a"), PathBuf::from("/b")])
      .expect("join_paths should succeed for two simple paths");
    let prev = std::env::var_os("LLAMASTASH_MODEL_PATHS");
    std::env::set_var("LLAMASTASH_MODEL_PATHS", &joined);
    let parsed = env_model_paths();
    match prev {
      Some(v) => std::env::set_var("LLAMASTASH_MODEL_PATHS", v),
      None => std::env::remove_var("LLAMASTASH_MODEL_PATHS"),
    }
    assert_eq!(parsed, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
  }

  #[test]
  fn env_model_paths_unset_returns_empty() {
    let prev = std::env::var_os("LLAMASTASH_MODEL_PATHS");
    std::env::remove_var("LLAMASTASH_MODEL_PATHS");
    let parsed = env_model_paths();
    if let Some(v) = prev {
      std::env::set_var("LLAMASTASH_MODEL_PATHS", v);
    }
    assert!(parsed.is_empty());
  }

  #[test]
  fn build_options_rejects_disable_scan_with_no_paths() {
    // The dead-end combo: scanning off, zero user paths anywhere.
    // Today this would leave the catalog empty forever — the
    // validator must turn it into a startup error so the user sees
    // *why* the daemon refused.
    let cli = parse_cli(&["--no-scan", "daemon", "start"]);
    let config = Config::default();
    let err = build_options(None, None, false, false, &cli, &config)
      .expect_err("--no-scan with zero paths must error");
    let msg = format!("{err:#}");
    assert!(
      msg.contains("scanning is disabled"),
      "error must name the failure mode, got: {msg}"
    );
  }

  #[test]
  fn build_options_accepts_disable_scan_when_cli_path_supplied() {
    let cli = parse_cli(&["--no-scan", "--model-path", "/work/keep", "daemon", "start"]);
    let config = Config::default();
    assert!(
      build_options(None, None, false, false, &cli, &config).is_ok(),
      "--no-scan + --model-path must build cleanly"
    );
  }

  #[test]
  fn build_options_accepts_disable_scan_when_config_path_supplied() {
    let cli = parse_cli(&["--no-scan", "daemon", "start"]);
    let config = Config {
      model_paths: vec![PathBuf::from("/work/cfg")],
      ..Config::default()
    };
    assert!(
      build_options(None, None, false, false, &cli, &config).is_ok(),
      "--no-scan + config model_paths must build cleanly"
    );
  }

  #[test]
  fn env_no_scan_accepts_documented_truthy_values() {
    // `1` is what the README documents; `true`/`yes`/`on` ride along
    // because every other LLAMASTASH_* bool in this binary accepts
    // them, and a script that already uses LLAMASTASH_OFFLINE=true
    // shouldn't be surprised when LLAMASTASH_NO_SCAN=true is rejected.
    let prev = std::env::var_os("LLAMASTASH_NO_SCAN");
    for value in &["1", "true", "TRUE", "yes", "On"] {
      std::env::set_var("LLAMASTASH_NO_SCAN", value);
      assert!(env_no_scan(), "value {value:?} should disable scan");
    }
    for value in &["0", "false", "no", "off", ""] {
      std::env::set_var("LLAMASTASH_NO_SCAN", value);
      assert!(!env_no_scan(), "value {value:?} should leave scan on");
    }
    std::env::remove_var("LLAMASTASH_NO_SCAN");
    assert!(!env_no_scan(), "unset should leave scan on");
    if let Some(v) = prev {
      std::env::set_var("LLAMASTASH_NO_SCAN", v);
    }
  }
}
