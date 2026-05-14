//! Daemon process: lockfile, socket bind, signal handling, accept loop.
//!
//! `run_foreground(opts)` does the whole lifecycle in the calling
//! process. `start_detached` re-execs the binary as a child with `setsid`
//! applied between `fork` and `exec`, then waits for the new daemon's
//! socket to become connectable before returning. The child is the daemon;
//! no in-runtime `fork()` is involved, which keeps the tokio runtime safe.

pub mod lockfile;
pub mod peercred;
pub mod server;
pub mod shutdown;

use std::{
  fs,
  path::{Path, PathBuf},
  time::Duration,
};

use anyhow::{anyhow, Context, Result};
use tokio::net::UnixListener;

use self::{
  lockfile::{acquire, AcquireOutcome},
  shutdown::{install_signal_handlers, ShutdownToken},
};
use crate::ipc::methods::MethodContext;

/// Options for starting the daemon. `state_dir` holds the PID lockfile;
/// `socket_path` is the Unix-domain socket the server binds to. Both
/// default to the OS-conventional paths via `util::paths`, but tests and
/// alternate deployments can override them.
#[derive(Debug, Clone)]
pub struct DaemonOptions {
  pub state_dir: PathBuf,
  pub socket_path: PathBuf,
}

impl DaemonOptions {
  /// Build options using the conventional XDG / macOS paths. Returns an
  /// error if the platform can't supply a state directory.
  pub fn from_defaults() -> Result<Self> {
    let state_dir = crate::util::paths::state_dir()
      .context("could not resolve a state directory for this platform")?;
    let socket_path = crate::util::paths::runtime_socket_path();
    Ok(Self {
      state_dir,
      socket_path,
    })
  }
}

/// Outcome of starting the daemon — surfaces the "another daemon is
/// already running" case so the CLI can exit 0 with a friendly message
/// rather than a generic error.
pub enum StartOutcome {
  /// Daemon ran to clean shutdown.
  RanToCompletion,
  /// Another instance is already running.
  AlreadyRunning(i32),
}

/// Run the daemon in the current process. Returns when the accept loop
/// exits (either via the `shutdown` method, SIGINT, or SIGTERM).
pub async fn run_foreground(opts: DaemonOptions) -> Result<StartOutcome> {
  // 1. PID lockfile.
  let lockfile = match acquire(&opts.state_dir).context("acquiring PID lockfile")? {
    AcquireOutcome::Acquired(lock) => lock,
    AcquireOutcome::AlreadyRunning { pid, .. } => return Ok(StartOutcome::AlreadyRunning(pid)),
  };

  // 2. Bind the Unix socket. A stale socket from a SIGKILL'd previous run
  // must be cleared, but only after we hold the lockfile — otherwise we
  // could race with a legitimate running daemon.
  if opts.socket_path.exists() {
    fs::remove_file(&opts.socket_path)
      .with_context(|| format!("removing stale socket at {}", opts.socket_path.display()))?;
  }
  ensure_parent_dir(&opts.socket_path)?;
  let listener = UnixListener::bind(&opts.socket_path)
    .with_context(|| format!("binding socket at {}", opts.socket_path.display()))?;
  apply_socket_permissions(&opts.socket_path)?;
  log::info!("daemon listening on {}", opts.socket_path.display());

  // 3. Shutdown plumbing.
  let token = ShutdownToken::new();
  let _signal_task = install_signal_handlers(token.clone());
  let ctx = MethodContext::new(token.clone());

  // 4. Accept loop until shutdown is triggered.
  let result = server::serve(listener, ctx).await;

  // 5. Cleanup. Lockfile cleans itself in Drop; the socket file is
  // removed here. We let the listener drop naturally.
  let _ = fs::remove_file(&opts.socket_path);
  drop(lockfile);

  result.map(|()| StartOutcome::RanToCompletion)
}

/// Re-exec the current binary as a detached daemon child and wait for it
/// to bind its socket. The parent returns to the user's shell once the
/// socket is connectable; the child is the long-lived daemon.
///
/// We deliberately do **not** call `fork()` ourselves: this function may
/// be invoked from inside a multithreaded tokio runtime, and `fork()` in
/// that situation only carries the calling thread into the child, leaving
/// any mutex held by another thread permanently locked. `Command::spawn`
/// with `pre_exec(setsid)` gets us a properly detached child without
/// touching the runtime.
///
/// Mechanism:
/// 1. Spawn `llamatui daemon start` (foreground mode) with `stdin`/
///    `stdout`/`stderr` redirected to `/dev/null` and `setsid` applied
///    between `fork` and `exec`.
/// 2. Poll the configured socket path for up to ~3s, attempting a
///    connection. Success → daemon is ready; return.
/// 3. If the child has already exited (e.g. AlreadyRunning), reap it and
///    surface its exit status.
#[cfg(unix)]
pub fn start_detached(opts: DaemonOptions) -> Result<StartOutcome> {
  let exe = std::env::current_exe().context("locating current executable for --detach")?;
  start_detached_with_exe(opts, exe)
}

/// Detached-start with an explicit executable path. Production callers
/// should use [`start_detached`], which resolves `current_exe()` itself.
/// Integration tests use this overload to point at the test binary so
/// they can exercise the full re-exec path against temp `DaemonOptions`.
#[cfg(unix)]
#[doc(hidden)]
pub fn start_detached_with_exe(opts: DaemonOptions, exe: PathBuf) -> Result<StartOutcome> {
  use std::{
    os::unix::process::CommandExt,
    process::{Command, Stdio},
  };

  // Fast path: a live daemon already owns the socket. Don't spawn a
  // child only to have it bail out — the parent would observe the
  // existing daemon's socket as "connectable" and falsely report success.
  if let Some(pid) = existing_daemon_pid(&opts.state_dir) {
    if std::os::unix::net::UnixStream::connect(&opts.socket_path).is_ok() {
      return Ok(StartOutcome::AlreadyRunning(pid));
    }
  }

  let mut cmd = Command::new(&exe);
  cmd
    .arg("daemon")
    .arg("start")
    // Propagate the caller-supplied paths to the re-exec'd child via
    // hidden flags. Without this, the child rebuilt `DaemonOptions`
    // from XDG defaults and silently ignored the parent's choices.
    .arg("--state-dir")
    .arg(&opts.state_dir)
    .arg("--socket-path")
    .arg(&opts.socket_path)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());

  // SAFETY: `pre_exec` runs in the child between fork and exec. We call
  // only async-signal-safe operations: `setsid` is on the POSIX
  // async-signal-safe list. No locks, allocations, or other tokio state
  // are touched here.
  unsafe {
    cmd.pre_exec(|| {
      if libc::setsid() < 0 {
        return Err(std::io::Error::last_os_error());
      }
      Ok(())
    });
  }

  let mut child = cmd.spawn().context("spawning detached daemon")?;

  // Poll for the socket to become connectable. Bail out early if the
  // child has already exited (most commonly AlreadyRunning).
  let deadline = std::time::Instant::now() + Duration::from_secs(3);
  loop {
    if let Some(status) = child.try_wait()? {
      // Child exited before socket appeared. If the lockfile exists and
      // points to a live pid, the child saw an existing daemon; we can
      // report that cleanly. Otherwise it's an unexpected failure.
      if let Some(pid) = existing_daemon_pid(&opts.state_dir) {
        return Ok(StartOutcome::AlreadyRunning(pid));
      }
      return Err(anyhow!(
        "detached daemon exited before binding socket (exit code: {:?})",
        status.code()
      ));
    }
    if std::os::unix::net::UnixStream::connect(&opts.socket_path).is_ok() {
      return Ok(StartOutcome::RanToCompletion);
    }
    if std::time::Instant::now() > deadline {
      // Don't leave the child orphaned if it's hung — kill and reap.
      let _ = child.kill();
      let _ = child.wait();
      return Err(anyhow!(
        "detached daemon did not bind socket within 3s ({})",
        opts.socket_path.display()
      ));
    }
    std::thread::sleep(Duration::from_millis(50));
  }
}

#[cfg(not(unix))]
pub fn start_detached(_opts: DaemonOptions) -> Result<StartOutcome> {
  Err(anyhow!("--detach is only supported on Unix targets"))
}

#[cfg(not(unix))]
#[doc(hidden)]
pub fn start_detached_with_exe(_opts: DaemonOptions, _exe: PathBuf) -> Result<StartOutcome> {
  Err(anyhow!("--detach is only supported on Unix targets"))
}

/// Returns the PID owning the daemon lockfile if (and only if) a live
/// process currently holds its `flock`. Used by `start_detached` to
/// short-circuit when an existing daemon already owns the socket.
///
/// Probing via `flock` rather than `kill(pid, 0)` matches `acquire`'s
/// ownership model: a recycled-PID collision can't masquerade as a live
/// daemon because the kernel released the lock when the original daemon
/// died, regardless of what the on-disk PID still says.
#[cfg(unix)]
fn existing_daemon_pid(state_dir: &Path) -> Option<i32> {
  use std::os::unix::io::AsRawFd;
  let pidfile = state_dir.join("daemon.pid");
  let file = std::fs::OpenOptions::new()
    .read(true)
    .write(true)
    .open(&pidfile)
    .ok()?;
  // SAFETY: `flock(2)` is a kernel syscall over a borrowed fd; no memory
  // is touched. `file` outlives the call.
  let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
  if ret == 0 {
    // We just acquired the lock — no daemon is running. Dropping `file`
    // closes the fd and releases the lock.
    return None;
  }
  // Lock contended → a daemon owns the pidfile. Read the recorded PID
  // for the friendly "already running" message; ownership is decided by
  // the lock, the PID value is informational.
  let raw = fs::read_to_string(&pidfile).ok()?;
  raw.trim().parse::<i32>().ok().filter(|p| *p > 0)
}

fn ensure_parent_dir(path: &Path) -> Result<()> {
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent)
      .with_context(|| format!("creating parent dir {}", parent.display()))?;
  }
  Ok(())
}

/// Apply mode `0600` to the socket file so other users on the host cannot
/// even open it. Peercred is the auth boundary that *catches* a bypass;
/// permissions are the boundary that *prevents* one.
#[cfg(unix)]
fn apply_socket_permissions(path: &Path) -> Result<()> {
  use std::os::unix::fs::PermissionsExt;
  fs::set_permissions(path, fs::Permissions::from_mode(0o600))
    .with_context(|| format!("chmod 0600 on {}", path.display()))?;
  Ok(())
}

#[cfg(not(unix))]
fn apply_socket_permissions(_path: &Path) -> Result<()> {
  Ok(())
}

// Re-export the symbols downstream callers reach for.
#[allow(unused_imports)]
pub use lockfile::AcquireOutcome as LockfileOutcome;
#[allow(unused_imports)]
pub use lockfile::Lockfile as DaemonLockfile;

/// Default drain timeout exposed for callers (tests, CLI status command).
pub const SHUTDOWN_DRAIN_TIMEOUT: Duration = server::DRAIN_TIMEOUT;
