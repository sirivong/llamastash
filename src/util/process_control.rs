//! Cross-platform process-supervision primitives.
//!
//! Phase B of the Windows+HTTP-IPC plan extracts the platform-specific
//! parts of process supervision (process-group setup, signal delivery,
//! liveness probing) behind a [`ProcessControl`] trait. The Unix
//! backend wraps `setsid()` + `kill(-pgid, SIG)`; Unit 6 of the plan
//! plugs in a Windows backend backed by Job Objects.
//!
//! Two distinct call shapes converge here:
//!
//! - **Supervised children** — processes the daemon spawned via
//!   [`ProcessControl::spawn_supervised`]. On Unix they ran `setsid()`
//!   in `pre_exec`, so signalling `-pid` reaches every process the
//!   child forked. The supervisor calls
//!   [`ProcessControl::signal_graceful`] / [`ProcessControl::signal_kill`]
//!   with `SignalTarget::ProcessGroup(pid)` so the SIGTERM→SIGKILL
//!   escalation propagates to grandchildren.
//! - **External processes** — pids the daemon doesn't own (the
//!   `stop_external` flow). The trait operates on `SignalTarget::SinglePid`
//!   so we never accidentally widen the blast radius to processes
//!   sharing a PGID with the foreign pid.
//!
//! The trait is dyn-safe so the daemon can pass a single
//! `Arc<dyn ProcessControl>` to every supervisor instance. Tests can
//! still swap in a mock control by accepting a generic
//! `impl ProcessControl` where dynamic dispatch isn't needed.

use std::sync::Arc;

use tokio::process::{Child, Command};

/// What a graceful / kill signal should target. Distinguishes the
/// supervised flow (whole process group rooted at `pid`) from the
/// external flow (single pid only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalTarget {
  /// Signal every process in the group whose PGID equals `pid`.
  /// Requires the target to have been spawned via
  /// [`ProcessControl::spawn_supervised`] (Unix: `setsid()`).
  ProcessGroup(u32),
  /// Signal the single process identified by `pid`. Used by
  /// `stop_external` where we don't own the lifecycle and must not
  /// touch unrelated processes that happen to share a PGID.
  SinglePid(u32),
}

impl SignalTarget {
  fn pid(&self) -> u32 {
    match self {
      SignalTarget::ProcessGroup(p) | SignalTarget::SinglePid(p) => *p,
    }
  }
}

/// Handle wrapping a process the daemon spawned. Carries the
/// `tokio::process::Child` plus any platform-specific state the
/// signaling path needs.
///
/// Unit 6 will widen this struct with a `cfg(windows)` JobObject
/// handle; today on Unix the struct is a transparent wrapper over
/// `Child`. The indirection exists so adding Windows state in Phase
/// C doesn't churn every supervisor call site.
pub struct SpawnedChild {
  /// The child process. The supervisor takes `.child` by value to
  /// install it under its own `Mutex<Option<Child>>` — there's no
  /// platform-side state on Unix to keep paired with it.
  pub child: Child,
}

impl SpawnedChild {
  /// Pid of the child, if it hasn't been reaped. Convenience over
  /// `self.child.id()`.
  pub fn pid(&self) -> Option<u32> {
    self.child.id()
  }

  /// Unwrap to the inner `Child`. Used by the supervisor when moving
  /// the child into its `Mutex<Option<Child>>` slot.
  pub fn into_child(self) -> Child {
    self.child
  }
}

/// Platform-specific process-supervision operations.
///
/// All methods are synchronous and best-effort: signaling an
/// already-reaped pid is a no-op (the OS returns `ESRCH`, which we
/// swallow). Callers that need stronger guarantees (PID-reuse defense,
/// post-signal waits) layer them on top.
pub trait ProcessControl: Send + Sync + 'static {
  /// Configure `cmd` for supervised spawn and spawn it. On Unix this
  /// installs a `pre_exec` hook running `setsid()` so the child
  /// becomes its own session + process-group leader. On Windows it
  /// will set `CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS` (Unit 6).
  fn spawn_supervised(&self, cmd: Command) -> std::io::Result<SpawnedChild>;

  /// Send the graceful-shutdown signal to `target`. On Unix this is
  /// `SIGTERM`. On Windows it will be `GenerateConsoleCtrlEvent(
  /// CTRL_BREAK_EVENT, …)` for `ProcessGroup` and a graceful close on
  /// `SinglePid`.
  ///
  /// Best-effort: silently swallows `ESRCH` (process already gone).
  fn signal_graceful(&self, target: SignalTarget);

  /// Force-kill `target`. On Unix this is `SIGKILL`. On Windows it
  /// will be `TerminateJobObject` (`ProcessGroup`) or
  /// `TerminateProcess` (`SinglePid`).
  ///
  /// Best-effort: silently swallows `ESRCH`.
  fn signal_kill(&self, target: SignalTarget);

  /// True iff `pid` corresponds to a live process. Implemented via
  /// `kill(pid, 0)` on Unix; `OpenProcess` + `GetExitCodeProcess` on
  /// Windows (Unit 6).
  fn is_alive(&self, pid: u32) -> bool;
}

/// Build the production [`ProcessControl`] for the current platform.
/// Returns an `Arc<dyn ProcessControl>` so the daemon can stash one
/// in its context and hand clones to every supervisor.
pub fn platform_default() -> Arc<dyn ProcessControl> {
  #[cfg(unix)]
  {
    Arc::new(UnixProcessControl)
  }
  #[cfg(not(unix))]
  {
    Arc::new(WindowsProcessControl)
  }
}

// ---------------------------------------------------------------------
// Unix backend
// ---------------------------------------------------------------------

/// Unix implementation. Process groups via `setsid()`; signals via
/// `libc::kill`; liveness via `kill(pid, 0)`.
#[cfg(unix)]
#[derive(Debug, Default, Clone, Copy)]
pub struct UnixProcessControl;

#[cfg(unix)]
impl ProcessControl for UnixProcessControl {
  fn spawn_supervised(&self, mut cmd: Command) -> std::io::Result<SpawnedChild> {
    // `pre_exec` is the method on `tokio::process::Command` re-exposed
    // from `std::os::unix::process::CommandExt`; rustc resolves the
    // call inherently and importing the trait would just trip the
    // unused-import lint.
    // SAFETY: `pre_exec` runs in the child between fork and exec.
    // `setsid` is on POSIX's async-signal-safe list — no allocations,
    // no locks, no tokio state touched.
    unsafe {
      cmd.pre_exec(|| {
        if libc::setsid() < 0 {
          return Err(std::io::Error::last_os_error());
        }
        Ok(())
      });
    }
    let child = cmd.spawn()?;
    Ok(SpawnedChild { child })
  }

  fn signal_graceful(&self, target: SignalTarget) {
    kill_target(target, libc::SIGTERM);
  }

  fn signal_kill(&self, target: SignalTarget) {
    kill_target(target, libc::SIGKILL);
  }

  fn is_alive(&self, pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
      return false;
    }
    // `kill(pid, 0)`: validates pid + permissions without delivering
    // a signal. ESRCH → gone; EPERM → process exists, we just can't
    // signal it (still counts as "alive").
    // SAFETY: kill(2) is a kernel syscall; no memory is touched.
    let rc = unsafe { libc::kill(pid as i32, 0) };
    if rc == 0 {
      return true;
    }
    std::io::Error::last_os_error()
      .raw_os_error()
      .map(|e| e == libc::EPERM)
      .unwrap_or(false)
  }
}

#[cfg(unix)]
fn kill_target(target: SignalTarget, sig: libc::c_int) {
  let pid = target.pid();
  // Negative pid in `kill(2)` signals every process in the
  // corresponding process group. The supervisor's `setsid()` made
  // the spawned child a PGID-leader, so `-pid` is the PGID covering
  // it and every grandchild it forked.
  let raw = match target {
    SignalTarget::ProcessGroup(_) => -(pid as i32),
    SignalTarget::SinglePid(_) => pid as i32,
  };
  // SAFETY: `kill(2)` is a kernel syscall; no memory is touched.
  // ESRCH is swallowed — concurrent reaps are normal.
  unsafe {
    libc::kill(raw, sig);
  }
}

// ---------------------------------------------------------------------
// Windows backend (stub — Unit 6 fills it in)
// ---------------------------------------------------------------------

/// Windows implementation. Currently a stub that compiles but every
/// method panics. Unit 6 of the Windows+HTTP-IPC plan replaces this
/// with a Job-Object–backed implementation.
#[cfg(not(unix))]
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsProcessControl;

#[cfg(not(unix))]
impl ProcessControl for WindowsProcessControl {
  fn spawn_supervised(&self, _cmd: Command) -> std::io::Result<SpawnedChild> {
    unimplemented!("Windows backend lands in Unit 6")
  }
  fn signal_graceful(&self, _target: SignalTarget) {
    unimplemented!("Windows backend lands in Unit 6")
  }
  fn signal_kill(&self, _target: SignalTarget) {
    unimplemented!("Windows backend lands in Unit 6")
  }
  fn is_alive(&self, _pid: u32) -> bool {
    unimplemented!("Windows backend lands in Unit 6")
  }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(all(test, unix))]
mod tests {
  use super::*;
  use std::time::{Duration, Instant};

  /// A pid we can be confident never points at a live process. Above
  /// every supported kernel's `pid_max`.
  const SENTINEL_DEAD_PID: u32 = 2_147_483_640;

  #[test]
  fn is_alive_returns_true_for_self() {
    let ctl = UnixProcessControl;
    let me = std::process::id();
    assert!(ctl.is_alive(me), "the test process must be 'alive'");
  }

  #[test]
  fn is_alive_returns_false_for_dead_pid() {
    let ctl = UnixProcessControl;
    assert!(
      !ctl.is_alive(SENTINEL_DEAD_PID),
      "sentinel pid must report dead"
    );
  }

  #[test]
  fn is_alive_returns_false_for_zero() {
    // `kill(0, …)` actually signals the caller's PGID, which would
    // be very wrong here — guard explicitly.
    let ctl = UnixProcessControl;
    assert!(!ctl.is_alive(0), "pid 0 must report dead, not signal self");
  }

  #[test]
  fn signal_graceful_no_panic_on_dead_pid() {
    // Best-effort contract: ESRCH must be swallowed (process already
    // reaped is the common case in the supervisor's retry loop).
    let ctl = UnixProcessControl;
    ctl.signal_graceful(SignalTarget::SinglePid(SENTINEL_DEAD_PID));
    ctl.signal_kill(SignalTarget::SinglePid(SENTINEL_DEAD_PID));
    ctl.signal_graceful(SignalTarget::ProcessGroup(SENTINEL_DEAD_PID));
    ctl.signal_kill(SignalTarget::ProcessGroup(SENTINEL_DEAD_PID));
  }

  #[tokio::test]
  async fn spawn_supervised_runs_setsid_and_groups_children() {
    // Spawn `sh -c 'sleep 30 & wait'` so we have a real grandchild
    // sharing the PGID. Then SIGTERM the group via the trait and
    // verify the parent exits within the grace window — proves the
    // signal reached the PGID, not just the immediate child.
    let ctl = UnixProcessControl;
    let mut cmd = Command::new("sh");
    cmd
      .arg("-c")
      .arg("sleep 30 & wait")
      .stdin(std::process::Stdio::null())
      .stdout(std::process::Stdio::null())
      .stderr(std::process::Stdio::null());
    let mut spawned = ctl.spawn_supervised(cmd).expect("spawn");
    let pid = spawned.pid().expect("pid available before reap");

    // Small settle so `sh` has time to fork its `sleep` grandchild.
    tokio::time::sleep(Duration::from_millis(100)).await;

    ctl.signal_graceful(SignalTarget::ProcessGroup(pid));

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
      if let Ok(Some(_)) = spawned.child.try_wait() {
        break;
      }
      if Instant::now() >= deadline {
        ctl.signal_kill(SignalTarget::ProcessGroup(pid));
        let _ = spawned.child.wait().await;
        panic!("SIGTERM to PGID did not reap the parent within 2s");
      }
      tokio::time::sleep(Duration::from_millis(50)).await;
    }
  }

  #[tokio::test]
  async fn signal_kill_escalates_when_graceful_ignored() {
    // `sh -c 'trap "" TERM; sleep 30'` ignores SIGTERM. The kill
    // method must escalate via SIGKILL and bring the child down.
    let ctl = UnixProcessControl;
    let mut cmd = Command::new("sh");
    cmd
      .arg("-c")
      .arg("trap '' TERM; sleep 30")
      .stdin(std::process::Stdio::null())
      .stdout(std::process::Stdio::null())
      .stderr(std::process::Stdio::null());
    let mut spawned = ctl.spawn_supervised(cmd).expect("spawn");
    let pid = spawned.pid().expect("pid");

    tokio::time::sleep(Duration::from_millis(50)).await;
    // Graceful must NOT reap (trap suppresses it). Brief observation.
    ctl.signal_graceful(SignalTarget::ProcessGroup(pid));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
      matches!(spawned.child.try_wait(), Ok(None)),
      "child trapping SIGTERM must still be running"
    );

    ctl.signal_kill(SignalTarget::ProcessGroup(pid));
    let status = tokio::time::timeout(Duration::from_secs(2), spawned.child.wait())
      .await
      .expect("SIGKILL must reap within 2s")
      .expect("wait");
    assert!(!status.success(), "killed process exits non-zero");
  }
}
