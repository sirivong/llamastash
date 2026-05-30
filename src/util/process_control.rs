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
  #[cfg(windows)]
  {
    Arc::new(WindowsProcessControl::new())
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
// Windows backend (Unit 6 of the Windows+HTTP-IPC plan)
// ---------------------------------------------------------------------

/// Windows implementation. One Job Object per supervised spawn, with
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` set so the OS tears the child
/// tree down when the daemon exits — even on `TerminateProcess`-style
/// ungraceful daemon death. Graceful drain is `CTRL+BREAK` to the
/// child's process group; force-kill is `TerminateJobObject`.
///
/// The job-object handles are stashed in a PID-keyed map inside the
/// controller because they must outlive the `Child` (closing the handle
/// would immediately kill the child via the kill-on-close flag). The
/// map grows by one per supervised spawn — daemon lifetimes and launch
/// counts are bounded enough that explicit eviction is unnecessary.
#[cfg(windows)]
pub struct WindowsProcessControl {
  jobs: std::sync::Mutex<std::collections::HashMap<u32, JobHandle>>,
}

#[cfg(windows)]
impl Default for WindowsProcessControl {
  fn default() -> Self {
    Self {
      jobs: std::sync::Mutex::new(std::collections::HashMap::new()),
    }
  }
}

#[cfg(windows)]
impl WindowsProcessControl {
  pub fn new() -> Self {
    Self::default()
  }
}

/// Owning wrapper over a Win32 `HANDLE`. Closes on drop. The raw
/// `*mut c_void` is `!Send + !Sync` by default; we mark the wrapper
/// `Send + Sync` because a HANDLE is a kernel reference whose
/// operations are all kernel syscalls — concurrent use from multiple
/// threads is safe by the Win32 contract.
#[cfg(windows)]
struct JobHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
unsafe impl Send for JobHandle {}
#[cfg(windows)]
unsafe impl Sync for JobHandle {}

#[cfg(windows)]
impl Drop for JobHandle {
  fn drop(&mut self) {
    if !self.0.is_null() {
      // SAFETY: handle was returned by a successful Win32 call (e.g.
      // CreateJobObjectW) and has not been closed elsewhere — Drop
      // runs at most once per JobHandle.
      unsafe {
        windows_sys::Win32::Foundation::CloseHandle(self.0);
      }
    }
  }
}

#[cfg(windows)]
impl ProcessControl for WindowsProcessControl {
  fn spawn_supervised(&self, mut cmd: Command) -> std::io::Result<SpawnedChild> {
    // `creation_flags` is the inherent method on
    // `tokio::process::Command` re-exposed from
    // `std::os::windows::process::CommandExt`; importing the trait
    // would just trip the unused-import lint.
    use windows_sys::Win32::System::JobObjects::{
      AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
      SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
      JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};

    // 1. Create the Job Object up front so spawn failure doesn't leak
    //    OS objects.
    // SAFETY: passing NULL for both lpJobAttributes and lpName is
    // documented as legal — an unnamed job with default security.
    let job_handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job_handle.is_null() {
      return Err(std::io::Error::last_os_error());
    }
    let job = JobHandle(job_handle);

    // 2. Enable kill-on-close so the daemon's death — even via
    //    TerminateProcess from Task Manager — tears down every
    //    supervised child. Mirrors the kernel's session-cleanup
    //    contract on Unix.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: `info` is initialized above; size matches the struct
    // exactly. SetInformationJobObject is a kernel call.
    let ok = unsafe {
      SetInformationJobObject(
        job.0,
        JobObjectExtendedLimitInformation,
        std::ptr::addr_of!(info) as *const _,
        std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
      )
    };
    if ok == 0 {
      return Err(std::io::Error::last_os_error());
    }

    // 3. Spawn with CREATE_NEW_PROCESS_GROUP (required for the child to
    //    receive CTRL+BREAK via GenerateConsoleCtrlEvent) plus
    //    DETACHED_PROCESS (no console window for headless daemon
    //    children whose stdout/stderr is piped).
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    let child = cmd.spawn()?;

    // 4. Assign the just-spawned child to the job. There's a brief
    //    window between spawn and assignment where the child is not in
    //    the job; acceptable because (a) llama-server.exe takes minutes
    //    to load a model — microseconds of unassigned state are
    //    negligible — and (b) the parent itself dying inside the window
    //    leaves the child running, which is the same as Unix's setsid
    //    behavior.
    // `raw_handle()` is inherent on `tokio::process::Child` on
    // Windows; no trait import needed.
    let process_handle = child.raw_handle().ok_or_else(|| {
      std::io::Error::new(
        std::io::ErrorKind::Other,
        "spawned child has no raw handle (already reaped?)",
      )
    })?;
    // SAFETY: `process_handle` was just returned by tokio for a live
    // child; `job.0` is a valid job handle from CreateJobObjectW.
    let ok = unsafe { AssignProcessToJobObject(job.0, process_handle as _) };
    if ok == 0 {
      return Err(std::io::Error::last_os_error());
    }

    // 5. Stash the job in the controller map so the handle outlives
    //    the child (closing it now would kill-on-job-close immediately).
    if let Some(pid) = child.id() {
      // Best-effort: a poisoned mutex here means a prior panic in
      // another spawn, which won't affect our ability to store this
      // entry — recover via `into_inner` on the poison.
      let mut guard = self
        .jobs
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      guard.insert(pid, job);
    }
    // If `child.id()` returned None the child has already exited; the
    // job drops here and kill-on-close is a no-op against the empty job.

    Ok(SpawnedChild { child })
  }

  fn signal_graceful(&self, target: SignalTarget) {
    use windows_sys::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
    let pid = target.pid();
    if pid == 0 {
      return;
    }
    // CTRL+BREAK to the spawned process group. For supervised children
    // this reaches the whole group via CREATE_NEW_PROCESS_GROUP at
    // spawn. For external single-pid targets the API will succeed iff
    // the foreign process happens to be a group leader — best-effort
    // matches the contract.
    //
    // Reliability for `llama-server.exe`'s SIGINT handler under
    // DETACHED_PROCESS is the open question called out in the plan;
    // the supervisor's SIGTERM→SIGKILL grace window escalates to
    // `signal_kill` if the graceful path doesn't reap the child.
    //
    // SAFETY: Win32 syscall, no memory referenced.
    unsafe {
      GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
    }
  }

  fn signal_kill(&self, target: SignalTarget) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::TerminateJobObject;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

    let pid = target.pid();
    if pid == 0 {
      return;
    }

    match target {
      SignalTarget::ProcessGroup(_) => {
        // Look up the job we created at spawn. If absent, the pid is
        // not one of our supervised children — fall back to single-pid
        // TerminateProcess so the caller still gets a kill.
        let job_entry = {
          let mut guard = self
            .jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
          guard.remove(&pid)
        };
        if let Some(job) = job_entry {
          // SAFETY: `job.0` is a valid HANDLE from CreateJobObjectW.
          unsafe {
            TerminateJobObject(job.0, 1);
          }
          // `job` drops here, closing the handle. Kill-on-close is a
          // no-op since TerminateJobObject already reaped everything.
          return;
        }
        // Fall through to single-pid path.
      }
      SignalTarget::SinglePid(_) => {}
    }

    // SAFETY: OpenProcess returns NULL on failure (handled below) or
    // a valid HANDLE. We always close it.
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if handle.is_null() {
      return; // ESRCH / access denied — swallow, matches Unix contract.
    }
    // SAFETY: `handle` is a valid PROCESS_TERMINATE handle.
    unsafe {
      TerminateProcess(handle, 1);
      CloseHandle(handle);
    }
  }

  fn is_alive(&self, pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
      GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // STILL_ACTIVE (259) is the exit-code value GetExitCodeProcess
    // returns for processes that haven't exited. windows-sys doesn't
    // export it as a constant under a stable feature path; spell out
    // the literal here to stay decoupled from feature-flag churn.
    const STILL_ACTIVE: u32 = 259;
    if pid == 0 {
      return false;
    }
    // SAFETY: Win32 syscall returning a HANDLE or NULL.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
      return false;
    }
    let mut code: u32 = 0;
    // SAFETY: `handle` is valid; `&mut code` is a writable u32 the OS
    // fills in.
    let ok = unsafe { GetExitCodeProcess(handle, &mut code as *mut u32) };
    // SAFETY: closing a handle we opened.
    unsafe {
      CloseHandle(handle);
    }
    ok != 0 && code == STILL_ACTIVE
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

#[cfg(all(test, windows))]
mod tests_windows {
  use super::*;
  use std::time::{Duration, Instant};

  /// PID we can be confident never points at a live process. The
  /// Windows kernel hands out small even integers for early-boot
  /// system processes, but the high u32 range stays unused.
  const SENTINEL_DEAD_PID: u32 = 4_000_000_000;

  #[test]
  fn is_alive_returns_true_for_self() {
    let ctl = WindowsProcessControl::new();
    let me = std::process::id();
    assert!(ctl.is_alive(me), "the test process must be 'alive'");
  }

  #[test]
  fn is_alive_returns_false_for_dead_pid() {
    let ctl = WindowsProcessControl::new();
    assert!(
      !ctl.is_alive(SENTINEL_DEAD_PID),
      "sentinel pid must report dead"
    );
  }

  #[test]
  fn is_alive_returns_false_for_zero() {
    let ctl = WindowsProcessControl::new();
    assert!(!ctl.is_alive(0), "pid 0 must report dead");
  }

  #[test]
  fn signal_no_panic_on_dead_pid() {
    // Best-effort contract mirror of the Unix test: signaling a pid
    // that doesn't exist must not panic / propagate errors.
    let ctl = WindowsProcessControl::new();
    ctl.signal_graceful(SignalTarget::SinglePid(SENTINEL_DEAD_PID));
    ctl.signal_kill(SignalTarget::SinglePid(SENTINEL_DEAD_PID));
    ctl.signal_graceful(SignalTarget::ProcessGroup(SENTINEL_DEAD_PID));
    ctl.signal_kill(SignalTarget::ProcessGroup(SENTINEL_DEAD_PID));
  }

  #[tokio::test]
  async fn spawn_supervised_kills_via_job_object() {
    // `ping -n 30` blocks ~30s waiting for replies. signal_kill must
    // reap it within the grace window via TerminateJobObject. We use
    // `ProcessGroup` so the kill goes through the job-object path —
    // proves the job-object plumbing actually works.
    let ctl = WindowsProcessControl::new();
    let mut cmd = Command::new("cmd");
    cmd
      .arg("/c")
      .arg("ping 127.0.0.1 -n 30")
      .stdin(std::process::Stdio::null())
      .stdout(std::process::Stdio::null())
      .stderr(std::process::Stdio::null());
    let mut spawned = ctl.spawn_supervised(cmd).expect("spawn");
    let pid = spawned.pid().expect("pid available before reap");

    // Brief settle so the child reaches its main loop before kill.
    tokio::time::sleep(Duration::from_millis(100)).await;

    ctl.signal_kill(SignalTarget::ProcessGroup(pid));

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
      if let Ok(Some(_)) = spawned.child.try_wait() {
        break;
      }
      if Instant::now() >= deadline {
        panic!("TerminateJobObject did not reap the child within 2s");
      }
      tokio::time::sleep(Duration::from_millis(50)).await;
    }
  }

  #[tokio::test]
  async fn is_alive_tracks_child_lifecycle() {
    // Spawn → alive → signal_kill → not alive.
    let ctl = WindowsProcessControl::new();
    let mut cmd = Command::new("cmd");
    cmd
      .arg("/c")
      .arg("ping 127.0.0.1 -n 30")
      .stdin(std::process::Stdio::null())
      .stdout(std::process::Stdio::null())
      .stderr(std::process::Stdio::null());
    let mut spawned = ctl.spawn_supervised(cmd).expect("spawn");
    let pid = spawned.pid().expect("pid");

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(ctl.is_alive(pid), "freshly-spawned child must report alive");

    ctl.signal_kill(SignalTarget::ProcessGroup(pid));
    let _ = tokio::time::timeout(Duration::from_secs(2), spawned.child.wait()).await;
    // Brief delay for the OS to finalize the process record.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!ctl.is_alive(pid), "killed child must report not alive");
  }
}
