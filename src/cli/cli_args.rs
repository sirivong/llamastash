//! Command-line argument schema (clap derive).
//!
//! Top-level args are `global = true` so every subcommand inherits them, and
//! a missing subcommand routes to the TUI. The shape of each subcommand
//! mirrors the agent-facing surface defined in
//! `docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md` (R35) —
//! handlers are stubbed until their respective implementation units land.

use std::{ffi::OsString, path::PathBuf};

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

use crate::banner::BANNER;

#[derive(Parser, Debug)]
#[command(
  name = "llamatui",
  version,
  about = "Fast keyboard-driven TUI + CLI for local llama.cpp models",
  long_about = None,
  before_help = BANNER,
)]
pub struct Cli {
  /// Path to a YAML config file (overrides `LLAMATUI_CONFIG`).
  #[arg(long, value_name = "PATH", global = true)]
  pub config: Option<PathBuf>,

  /// Path to the `llama-server` binary (overrides `LLAMATUI_LLAMA_SERVER`).
  #[arg(long, value_name = "PATH", global = true)]
  pub llama_server: Option<PathBuf>,

  /// Extra directory to scan for GGUF models. Repeatable.
  #[arg(short = 'p', long = "model-path", value_name = "DIR", global = true)]
  pub model_paths: Vec<PathBuf>,

  /// Disable filesystem scanning of default and configured roots.
  #[arg(long, global = true)]
  pub no_scan: bool,

  /// Verbose logging (Debug level instead of Info).
  #[arg(short, long, global = true, action = ArgAction::SetTrue)]
  pub verbose: bool,

  #[command(subcommand)]
  pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
  /// Manage the background daemon (auto-spawned on attach).
  #[command(subcommand)]
  Daemon(DaemonAction),
  /// List discovered models.
  List(ListArgs),
  /// Start a model.
  Start(StartArgs),
  /// Stop a running model.
  Stop(StopArgs),
  /// Show daemon and running-model status.
  Status(StatusArgs),
  /// Tail or follow a running model's log.
  Logs(LogsArgs),
  /// Manage named launch presets for a model.
  Presets(PresetsArgs),
  /// Pull a GGUF from HuggingFace.
  Pull(PullArgs),
}

#[derive(Subcommand, Debug)]
pub enum DaemonAction {
  /// Start the daemon (no-op if already running).
  Start {
    /// Background the daemon by detaching from the controlling terminal.
    #[arg(long)]
    detach: bool,
  },
  /// Stop the running daemon. Running models keep running.
  Stop,
  /// Print daemon PID, uptime, and connected-client count.
  Status,
}

#[derive(Args, Debug)]
pub struct ListArgs {
  /// Emit JSON instead of the human-readable table.
  #[arg(long)]
  pub json: bool,
  /// Filter substring matched against name, path, arch, and quant.
  #[arg(long, value_name = "PATTERN")]
  pub filter: Option<String>,
}

#[derive(Args, Debug)]
pub struct StartArgs {
  /// Model reference: name substring, absolute path, or canonical model id.
  pub model: String,
  /// Saved preset to load before applying overrides.
  #[arg(long, value_name = "NAME")]
  pub preset: Option<String>,
  /// Context length override.
  #[arg(long, value_name = "TOKENS")]
  pub ctx: Option<u32>,
  /// Pin the listening port (otherwise auto-allocated from the config range).
  #[arg(long, value_name = "PORT")]
  pub port: Option<u16>,
  /// Enable or disable reasoning (bundles `--reasoning-format deepseek --jinja`
  /// + smoke-test `<think>` collapse). Advanced panel can unbundle.
  #[arg(long, value_enum)]
  pub reasoning: Option<ReasoningFlag>,
  /// Force the launch mode. Defaults to the value detected from GGUF metadata.
  #[arg(long, value_enum)]
  pub mode: Option<LaunchMode>,
  /// Extra flags forwarded verbatim to `llama-server` after `--`.
  #[arg(last = true, value_name = "ARG")]
  pub extra: Vec<OsString>,
}

#[derive(Args, Debug)]
pub struct StopArgs {
  /// Model id or port to stop. Required unless `--all` is set.
  pub target: Option<String>,
  /// Stop every model owned by this daemon.
  #[arg(long, conflicts_with = "target")]
  pub all: bool,
  /// Skip the confirmation prompt.
  #[arg(short, long)]
  pub yes: bool,
}

#[derive(Args, Debug)]
pub struct StatusArgs {
  /// Emit JSON instead of the human-readable status block.
  #[arg(long)]
  pub json: bool,
}

#[derive(Args, Debug)]
pub struct LogsArgs {
  /// Model id whose log to tail.
  pub id: String,
  /// Follow the log instead of printing the current tail.
  #[arg(short, long)]
  pub follow: bool,
  /// Number of trailing lines to print before following.
  #[arg(short = 'n', long, value_name = "N")]
  pub lines: Option<u32>,
}

#[derive(Args, Debug)]
pub struct PresetsArgs {
  /// Model reference: name substring, absolute path, or canonical model id.
  pub model: String,
  #[command(subcommand)]
  pub action: PresetsAction,
}

#[derive(Subcommand, Debug)]
pub enum PresetsAction {
  /// List saved presets for this model.
  List,
  /// Save the current/passed launch params under `name`.
  Save {
    name: String,
    #[arg(long, value_name = "TOKENS")]
    ctx: Option<u32>,
    #[arg(long, value_enum)]
    reasoning: Option<ReasoningFlag>,
    #[arg(long, value_enum)]
    mode: Option<LaunchMode>,
    #[arg(last = true, value_name = "ARG")]
    extra: Vec<OsString>,
  },
  /// Delete a saved preset.
  Delete { name: String },
  /// Print a saved preset's parameters.
  Show { name: String },
}

#[derive(Args, Debug)]
pub struct PullArgs {
  /// HuggingFace repo id, optionally with `:filename.gguf` to pin a single file.
  pub repo: String,
  /// Fire-and-forget mode: return immediately, monitor with `llamatui status`.
  #[arg(long)]
  pub background: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lower")]
pub enum ReasoningFlag {
  On,
  Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lower")]
pub enum LaunchMode {
  Chat,
  Embedding,
  Rerank,
}

#[cfg(test)]
mod tests {
  use super::*;

  fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(std::iter::once("llamatui").chain(args.iter().copied()))
      .expect("argv should parse")
  }

  #[test]
  fn parses_with_no_subcommand() {
    let cli = parse(&[]);
    assert!(cli.command.is_none());
    assert!(!cli.no_scan);
    assert!(!cli.verbose);
  }

  #[test]
  fn model_path_is_repeatable() {
    let cli = parse(&["--model-path", "/a", "--model-path", "/b", "-p", "/c"]);
    assert_eq!(
      cli.model_paths,
      vec![
        PathBuf::from("/a"),
        PathBuf::from("/b"),
        PathBuf::from("/c"),
      ]
    );
  }

  #[test]
  fn global_flags_work_before_and_after_subcommand() {
    let before = parse(&["--no-scan", "list"]);
    let after = parse(&["list", "--no-scan"]);
    assert!(before.no_scan);
    assert!(after.no_scan);
    assert!(matches!(before.command, Some(Command::List(_))));
    assert!(matches!(after.command, Some(Command::List(_))));
  }

  #[test]
  fn daemon_subcommands_parse() {
    let cli_start = parse(&["daemon", "start", "--detach"]);
    match cli_start.command {
      Some(Command::Daemon(DaemonAction::Start { detach })) => assert!(detach),
      other => panic!("expected daemon start --detach, got {other:?}"),
    }

    let cli_stop = parse(&["daemon", "stop"]);
    assert!(matches!(
      cli_stop.command,
      Some(Command::Daemon(DaemonAction::Stop))
    ));

    let cli_status = parse(&["daemon", "status"]);
    assert!(matches!(
      cli_status.command,
      Some(Command::Daemon(DaemonAction::Status))
    ));
  }

  #[test]
  fn daemon_without_action_is_an_error() {
    let result = Cli::try_parse_from(["llamatui", "daemon"]);
    assert!(result.is_err(), "daemon subcommand requires an action");
  }

  #[test]
  fn start_accepts_full_launch_surface() {
    let cli = parse(&[
      "start",
      "qwen-coder",
      "--preset",
      "coding",
      "--ctx",
      "32768",
      "--port",
      "41150",
      "--reasoning",
      "on",
      "--mode",
      "chat",
      "--",
      "--threads",
      "8",
    ]);
    match cli.command {
      Some(Command::Start(args)) => {
        assert_eq!(args.model, "qwen-coder");
        assert_eq!(args.preset.as_deref(), Some("coding"));
        assert_eq!(args.ctx, Some(32768));
        assert_eq!(args.port, Some(41150));
        assert_eq!(args.reasoning, Some(ReasoningFlag::On));
        assert_eq!(args.mode, Some(LaunchMode::Chat));
        assert_eq!(
          args.extra,
          vec![OsString::from("--threads"), OsString::from("8")]
        );
      }
      other => panic!("expected Start, got {other:?}"),
    }
  }

  #[test]
  fn stop_all_conflicts_with_target() {
    let result = Cli::try_parse_from(["llamatui", "stop", "42", "--all"]);
    assert!(result.is_err());
  }

  #[test]
  fn list_supports_json_and_filter() {
    let cli = parse(&["list", "--json", "--filter", "qwen"]);
    match cli.command {
      Some(Command::List(args)) => {
        assert!(args.json);
        assert_eq!(args.filter.as_deref(), Some("qwen"));
      }
      other => panic!("expected List, got {other:?}"),
    }
  }

  #[test]
  fn presets_save_parses_with_extra_args() {
    let cli = parse(&[
      "presets",
      "qwen-coder",
      "save",
      "long-ctx",
      "--ctx",
      "131072",
      "--",
      "--flash-attn",
    ]);
    match cli.command {
      Some(Command::Presets(args)) => {
        assert_eq!(args.model, "qwen-coder");
        match args.action {
          PresetsAction::Save {
            name, ctx, extra, ..
          } => {
            assert_eq!(name, "long-ctx");
            assert_eq!(ctx, Some(131072));
            assert_eq!(extra, vec![OsString::from("--flash-attn")]);
          }
          other => panic!("expected Save, got {other:?}"),
        }
      }
      other => panic!("expected Presets, got {other:?}"),
    }
  }

  #[test]
  fn pull_parses() {
    let cli = parse(&[
      "pull",
      "Qwen/Qwen2.5-Coder-7B-Instruct-GGUF",
      "--background",
    ]);
    match cli.command {
      Some(Command::Pull(args)) => {
        assert_eq!(args.repo, "Qwen/Qwen2.5-Coder-7B-Instruct-GGUF");
        assert!(args.background);
      }
      other => panic!("expected Pull, got {other:?}"),
    }
  }

  #[test]
  fn logs_follow_and_tail_lines() {
    let cli = parse(&["logs", "model-abc", "-f", "-n", "200"]);
    match cli.command {
      Some(Command::Logs(args)) => {
        assert_eq!(args.id, "model-abc");
        assert!(args.follow);
        assert_eq!(args.lines, Some(200));
      }
      other => panic!("expected Logs, got {other:?}"),
    }
  }

  #[test]
  fn unknown_reasoning_value_errors() {
    let result = Cli::try_parse_from(["llamatui", "start", "x", "--reasoning", "maybe"]);
    assert!(result.is_err());
  }

  #[test]
  fn version_flag_works() {
    let result = Cli::try_parse_from(["llamatui", "--version"]);
    // clap returns an "error" with exit kind DisplayVersion for --version.
    let err = result.unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
    let rendered = err.to_string();
    assert!(rendered.contains(env!("CARGO_PKG_VERSION")));
  }

  #[test]
  fn help_flag_lists_every_subcommand() {
    let result = Cli::try_parse_from(["llamatui", "--help"]);
    let err = result.unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    let rendered = err.to_string();
    for sub in [
      "daemon", "list", "start", "stop", "status", "logs", "presets", "pull",
    ] {
      assert!(
        rendered.contains(sub),
        "help output should list `{sub}` subcommand, got: {rendered}"
      );
    }
  }
}
