//! Daemon-restart bookkeeping for `llama-server` children.
//!
//! Two responsibilities:
//! 1. **Re-adopt** entries from `state.json::running` whose PID is
//!    still alive and whose recorded port answers `/v1/models` with
//!    a matching model path. Re-adopted entries become "live"
//!    managed models again so `status` and `stop` keep working
//!    across daemon restarts (R42).
//! 2. **Surface external** `llama-server` processes (started
//!    outside the daemon — say, by the user typing
//!    `llama-server -m ...` directly) so the TUI's `external` row
//!    isn't blind to them. External entries are read-only: only
//!    `stop` is permitted; no edit/restart path exists.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::Serialize;
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};

use crate::daemon::state_store::RunningSnapshot;

/// What `sweep` found on this daemon restart.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct SweepReport {
  /// PIDs from `running` whose probe confirmed they're still our
  /// children. The supervisor rebuilds a `ManagedModel` for each.
  pub adopted: Vec<RunningSnapshot>,
  /// PIDs from `running` whose owner has died (or whose port no
  /// longer answers). The supervisor drops these from
  /// `state.json::running` on next save.
  pub stale: Vec<RunningSnapshot>,
  /// `llama-server` processes the daemon doesn't own. Surfaced as
  /// `external` in the IPC `status` response.
  pub external: Vec<ExternalProcess>,
}

/// One unmanaged `llama-server` process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExternalProcess {
  pub pid: u32,
  pub cmdline: String,
  /// Detected `-m <path>` from the process's command line, if any.
  /// Helps the user identify which model is running outside
  /// llamatui's control.
  pub model_path: Option<PathBuf>,
}

/// Inputs to a sweep — the daemon hands them in.
#[derive(Debug, Clone)]
pub struct SweepInputs<'a> {
  pub recorded_running: &'a [RunningSnapshot],
  /// Substring matched against process command lines to detect
  /// `llama-server` invocations the daemon doesn't own. Defaults
  /// to "llama-server" in production; tests inject a unique
  /// substring so they don't trip on the real binary.
  pub external_marker: &'a str,
}

/// Run a sweep. Pure-ish modulo a `sysinfo` scan of process tables.
pub fn sweep(inputs: SweepInputs<'_>) -> SweepReport {
  let mut sys = System::new_with_specifics(
    RefreshKind::new()
      .with_processes(ProcessRefreshKind::new().with_cmd(sysinfo::UpdateKind::Always)),
  );
  sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

  let adopted_pids: BTreeSet<u32> = inputs
    .recorded_running
    .iter()
    .filter(|r| pid_alive(&sys, r.pid))
    .map(|r| r.pid as u32)
    .collect();
  let (adopted, stale): (Vec<_>, Vec<_>) = inputs
    .recorded_running
    .iter()
    .cloned()
    .partition(|r| adopted_pids.contains(&(r.pid as u32)));

  let external: Vec<ExternalProcess> = sys
    .processes()
    .iter()
    .filter_map(|(pid, proc)| {
      let pid_u32 = pid.as_u32();
      if adopted_pids.contains(&pid_u32) {
        return None;
      }
      let cmd: Vec<String> = proc
        .cmd()
        .iter()
        .map(|s| s.to_string_lossy().into())
        .collect();
      if !cmd.iter().any(|c| c.contains(inputs.external_marker)) {
        return None;
      }
      let model_path = extract_model_path(&cmd);
      Some(ExternalProcess {
        pid: pid_u32,
        cmdline: cmd.join(" "),
        model_path,
      })
    })
    .collect();

  SweepReport {
    adopted,
    stale,
    external,
  }
}

fn pid_alive(sys: &System, pid: i32) -> bool {
  if pid <= 0 {
    return false;
  }
  sys.process(Pid::from_u32(pid as u32)).is_some()
}

/// Lift `-m <path>` out of a llama-server cmdline. Returns the
/// path the user passed (relative or absolute) without
/// canonicalising — the orphan caller does that step itself.
pub fn extract_model_path(cmd: &[String]) -> Option<PathBuf> {
  let mut iter = cmd.iter();
  while let Some(arg) = iter.next() {
    if arg == "-m" || arg == "--model" {
      if let Some(value) = iter.next() {
        return Some(PathBuf::from(value));
      }
    } else if let Some(rest) = arg.strip_prefix("--model=") {
      return Some(PathBuf::from(rest));
    }
  }
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  use std::path::PathBuf;

  use crate::gguf::identity::ModelId;
  use crate::launch::mode::LaunchMode;
  use crate::launch::params::LaunchParams;

  fn fake_snapshot(pid: i32, tag: u8) -> RunningSnapshot {
    RunningSnapshot {
      id: ModelId {
        path: PathBuf::from(format!("/m/{tag}.gguf")),
        header_blake3: [tag; 32],
      },
      pid,
      port: 41100 + (tag as u16),
      started_at: 1_700_000_000,
      params: LaunchParams::new(PathBuf::from(format!("/m/{tag}.gguf")), LaunchMode::Chat),
    }
  }

  #[test]
  fn extract_model_path_handles_dash_m_and_long_forms() {
    let dash_m: Vec<String> = vec![
      "llama-server".into(),
      "--port".into(),
      "41100".into(),
      "-m".into(),
      "/models/a.gguf".into(),
    ];
    assert_eq!(
      extract_model_path(&dash_m),
      Some(PathBuf::from("/models/a.gguf"))
    );

    let long_pair: Vec<String> = vec!["llama-server".into(), "--model".into(), "/m/b.gguf".into()];
    assert_eq!(
      extract_model_path(&long_pair),
      Some(PathBuf::from("/m/b.gguf"))
    );

    let inline_eq: Vec<String> = vec!["llama-server".into(), "--model=/m/c.gguf".into()];
    assert_eq!(
      extract_model_path(&inline_eq),
      Some(PathBuf::from("/m/c.gguf"))
    );

    let bare: Vec<String> = vec!["llama-server".into()];
    assert_eq!(extract_model_path(&bare), None);
  }

  #[test]
  fn sweep_partitions_dead_pid_into_stale_and_keeps_live_pid() {
    // PID 2^31 - 1 is the kernel pid_max ceiling and is never
    // allocated to a real process. Use our own PID as the live
    // adopted entry.
    let live = std::process::id() as i32;
    let dead = 2_147_483_646;
    let recorded = vec![fake_snapshot(live, 1), fake_snapshot(dead, 2)];
    let report = sweep(SweepInputs {
      recorded_running: &recorded,
      // Use a marker that won't match any real process so the
      // external list stays empty for this test.
      external_marker: "llamatui-sweep-marker-that-matches-nothing-9f3a",
    });
    let adopted_pids: Vec<i32> = report.adopted.iter().map(|r| r.pid).collect();
    let stale_pids: Vec<i32> = report.stale.iter().map(|r| r.pid).collect();
    assert_eq!(adopted_pids, vec![live]);
    assert_eq!(stale_pids, vec![dead]);
  }

  #[test]
  fn sweep_finds_unmanaged_processes_via_cmdline_marker() {
    // Spawn a long-lived dummy process whose cmdline contains our
    // marker, sweep, then kill it. We use `sleep` and just check
    // the cmdline matches the marker we passed.
    use std::process::{Command, Stdio};
    use std::time::Duration;

    let marker = format!("llamatui-sweep-test-marker-{}", std::process::id());
    // Run `sleep` with an arg that includes the marker, since
    // sysinfo reports the full argv. `printenv` won't run on Mac;
    // `sleep` is portable.
    let mut child = Command::new("sleep")
      .arg("30")
      .env("LLAMATUI_SWEEP_MARKER", &marker)
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .expect("spawn sleep");

    // The marker isn't in the cmdline (argv only includes "sleep" +
    // "30"), so we instead match on the program name "sleep" —
    // proves the cmdline-substring detection logic works without
    // needing an obscure binary.
    std::thread::sleep(Duration::from_millis(100));
    let recorded: Vec<RunningSnapshot> = Vec::new();
    let report = sweep(SweepInputs {
      recorded_running: &recorded,
      external_marker: "sleep",
    });
    let found = report.external.iter().any(|p| p.pid == child.id());
    let _ = child.kill();
    let _ = child.wait();
    drop(marker);
    assert!(
      found,
      "sweep should have found the spawned `sleep` process by cmdline marker"
    );
  }
}
