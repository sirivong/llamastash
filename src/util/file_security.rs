//! Cross-platform secret-file hardening.
//!
//! `runtime.json` and `state.json` carry per-daemon secrets (the bearer
//! token, model paths, last-launch params) and must not be readable by
//! other accounts on the machine. On Unix this is the explicit
//! `chmod 0o600` applied during atomic-write. On Windows there's no
//! mode-bit equivalent; we apply a Protected DACL that grants
//! Generic-All to the file owner and no inheritance from the parent.
//!
//! Unix mode-bit application lives in [`crate::util::atomic_write`].
//! This module only contains the Windows surface; on Unix the
//! `set_owner_only_dacl` function is a no-op compile-time stub.
//!
//! The SDDL string `D:P(A;;GA;;;OW)` expands as:
//! - `D:` — DACL section
//! - `P` — Protected (no inheritance from the parent)
//! - `(A;;GA;;;OW)` — Allow ACE granting Generic All to the OWner
//!
//! Best-effort by design: a failure to apply the DACL is logged and
//! swallowed. The state directory is already under `%LOCALAPPDATA%`,
//! which inherits a per-user ACL, so the file is non-readable to
//! other users even without explicit hardening. The DACL apply here
//! is belt-and-suspenders against misconfigured parent ACLs.

use std::path::Path;

/// Apply an owner-only DACL to `path` on Windows (no-op on Unix —
/// caller's `chmod` already hardened the file). Best-effort: failures
/// are swallowed with a warning. Safe to call repeatedly.
#[cfg(windows)]
pub fn set_owner_only_dacl(path: &Path) {
  use std::os::windows::ffi::OsStrExt;
  use windows_sys::Win32::Foundation::LocalFree;
  use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
  };
  use windows_sys::Win32::Security::{SetFileSecurityW, DACL_SECURITY_INFORMATION};

  // Owner = Generic All, Protected DACL (no inheritance from parent).
  // OW = owner of the object (current user creating the file).
  let sddl: Vec<u16> = "D:P(A;;GA;;;OW)\0".encode_utf16().collect();
  let mut sd_ptr: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR = std::ptr::null_mut();
  // SAFETY: SDDL is a NUL-terminated UTF-16 string; sd_ptr receives a
  // freshly-allocated security descriptor we LocalFree below.
  let ok = unsafe {
    ConvertStringSecurityDescriptorToSecurityDescriptorW(
      sddl.as_ptr(),
      SDDL_REVISION_1 as u32,
      &mut sd_ptr,
      std::ptr::null_mut(),
    )
  };
  if ok == 0 {
    log::warn!(
      "could not build security descriptor for {}: {}",
      path.display(),
      std::io::Error::last_os_error()
    );
    return;
  }
  let path_w: Vec<u16> = path
    .as_os_str()
    .encode_wide()
    .chain(std::iter::once(0))
    .collect();
  // SAFETY: sd_ptr is a valid SD from the call above; path_w is
  // NUL-terminated UTF-16; SetFileSecurityW reads both and returns
  // BOOL.
  let ok = unsafe { SetFileSecurityW(path_w.as_ptr(), DACL_SECURITY_INFORMATION, sd_ptr) };
  if ok == 0 {
    log::warn!(
      "could not apply owner-only DACL to {}: {}",
      path.display(),
      std::io::Error::last_os_error()
    );
  }
  // SAFETY: sd_ptr was allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW
  // per its documented contract: caller must LocalFree.
  unsafe {
    LocalFree(sd_ptr as _);
  }
}

#[cfg(not(windows))]
pub fn set_owner_only_dacl(_path: &Path) {
  // Unix files are hardened via `chmod 0o600` in atomic_write::write_secure.
}

#[cfg(all(test, windows))]
mod tests_windows {
  use super::*;
  use std::time::{SystemTime, UNIX_EPOCH};

  fn temp_path(label: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .expect("clock")
      .as_nanos();
    let p = std::env::temp_dir().join(format!(
      "llamastash-dacl-{label}-{}-{nanos}.tmp",
      std::process::id()
    ));
    std::fs::write(&p, b"secret").expect("seed file");
    p
  }

  #[test]
  fn set_owner_only_dacl_no_panic_on_existing_file() {
    // We don't try to verify the resulting DACL programmatically here —
    // that requires more Win32 plumbing than the test deserves. The
    // contract is "best-effort, no panic, no harm to the file";
    // verifying the call returns without panicking and that the file
    // is still readable is enough.
    let p = temp_path("apply");
    set_owner_only_dacl(&p);
    let read = std::fs::read(&p).expect("file still readable by owner");
    assert_eq!(read, b"secret");
    std::fs::remove_file(&p).ok();
  }

  #[test]
  fn set_owner_only_dacl_no_panic_on_missing_file() {
    // Best-effort contract: missing file just logs a warning.
    let p = temp_path("missing");
    std::fs::remove_file(&p).expect("remove");
    set_owner_only_dacl(&p);
  }
}
