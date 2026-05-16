//! Unix privilege drop (`user`) and file-descriptor limit (`worker_rlimit_nofile`).
//!
//! Why this exists: Nginx-style deployments bind privileged ports
//! (`:80` / `:443`) which requires root or `CAP_NET_BIND_SERVICE`, but
//! every operations team will refuse to leave the runtime as `uid 0`.
//! The standard pattern is "bind first, then drop." This module
//! provides exactly that.
//!
//! Order matters and is enforced by the call sites in [`crate::main`]:
//!
//! 1. raise `RLIMIT_NOFILE` (only root can lift the *hard* cap)
//! 2. open log files, write the PID file
//! 3. bind every socket (listeners + QUIC endpoints)
//! 4. **drop privileges** — `initgroups` → `setgid` → `setuid`
//!
//! After step 4 the process can no longer rebind privileged ports
//! (a `SIGHUP` reload that asks to bind a new `:80` listener will
//! fail with EACCES — that is fine and we log it loudly).

#![cfg(unix)]

use std::ffi::CString;
use std::io;

/// A resolved (uid, gid) pair, plus the original /etc/passwd entry's
/// primary group (used by `initgroups` to set the supplementary list).
#[derive(Debug)]
pub struct UserIds {
    pub uid: libc::uid_t,
    pub gid: libc::gid_t,
    /// The username — needed by `initgroups(3)` to look up supplementary
    /// groups via the `/etc/group` database.
    pub user_cstr: CString,
}

/// Resolve `user` (and optional `group`) from /etc/passwd and
/// /etc/group. If `group` is `None`, the user's primary group is used.
pub fn resolve(user: &str, group: Option<&str>) -> Result<UserIds, String> {
    let user_cstr = CString::new(user)
        .map_err(|_| format!("user name '{user}' contains an internal NUL byte"))?;

    // SAFETY: `getpwnam` returns a pointer to thread-local-or-static
    // memory we do not own; we copy out the fields we need and drop
    // the pointer immediately.
    let pwd = unsafe { libc::getpwnam(user_cstr.as_ptr()) };
    if pwd.is_null() {
        return Err(format!("user '{user}' not found in /etc/passwd"));
    }
    let (uid, primary_gid) = unsafe { ((*pwd).pw_uid, (*pwd).pw_gid) };

    let gid = if let Some(g) = group {
        let g_cstr = CString::new(g)
            .map_err(|_| format!("group name '{g}' contains an internal NUL byte"))?;
        let gr = unsafe { libc::getgrnam(g_cstr.as_ptr()) };
        if gr.is_null() {
            return Err(format!("group '{g}' not found in /etc/group"));
        }
        unsafe { (*gr).gr_gid }
    } else {
        primary_gid
    };

    Ok(UserIds {
        uid,
        gid,
        user_cstr,
    })
}

/// Drop the process's effective and real uid/gid to the resolved
/// [`UserIds`]. The call order — `initgroups` → `setgid` → `setuid` —
/// matters: once `setuid` runs we lose the privilege to change group
/// membership.
///
/// Returns `Ok(())` on success; `Err` carries the failing syscall and
/// errno. Callers should treat any failure here as fatal — staying as
/// root despite an explicit `user` directive would be a silent
/// security regression.
pub fn drop_to(ids: &UserIds) -> Result<(), String> {
    // The `gid` parameter type of `initgroups` differs by libc; cast
    // through a single `c_int` for portability.
    let r = unsafe { libc::initgroups(ids.user_cstr.as_ptr(), ids.gid as _) };
    if r != 0 {
        return Err(format!(
            "initgroups(gid={}): {}",
            ids.gid,
            io::Error::last_os_error()
        ));
    }
    if unsafe { libc::setgid(ids.gid) } != 0 {
        return Err(format!(
            "setgid({}): {}",
            ids.gid,
            io::Error::last_os_error()
        ));
    }
    if unsafe { libc::setuid(ids.uid) } != 0 {
        return Err(format!(
            "setuid({}): {}",
            ids.uid,
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Raise the *soft* `RLIMIT_NOFILE` to `target`, clamped to the
/// current *hard* limit (only root can raise the hard cap, and we
/// don't try). Returns `(old_soft, new_soft)` on success.
pub fn raise_nofile(target: u64) -> Result<(u64, u64), String> {
    let mut rlim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) } != 0 {
        return Err(format!("getrlimit(NOFILE): {}", io::Error::last_os_error()));
    }
    let old_soft = rlim.rlim_cur as u64;
    let want = target.min(rlim.rlim_max as u64);
    // Already at or above the requested soft limit — nothing to do.
    if old_soft >= want {
        return Ok((old_soft, old_soft));
    }
    rlim.rlim_cur = want as _;
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) } != 0 {
        return Err(format!("setrlimit(NOFILE): {}", io::Error::last_os_error()));
    }
    Ok((old_soft, want))
}

/// Returns `true` if the process is currently running as root.
pub fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_missing_user_errors() {
        let err = resolve("definitely-no-such-user-12345", None).unwrap_err();
        assert!(err.contains("not found"), "got: {err}");
    }

    #[test]
    fn resolve_missing_group_errors() {
        // Pick a user that definitely exists on every Unix system.
        // We don't care about the result of looking *that* up; we care
        // that the bogus group fails loudly.
        let err = resolve("root", Some("definitely-no-such-group-12345"))
            .unwrap_err();
        assert!(
            err.contains("not found in /etc/group"),
            "got: {err}"
        );
    }

    #[test]
    fn raise_nofile_clamps_to_hard_cap() {
        // Asking for a ridiculous number must not panic; it should
        // either succeed (clamped) or return an OS error. We assert
        // the no-panic + plausible-shape contract.
        let r = raise_nofile(u64::MAX);
        match r {
            Ok((_, new)) => assert!(new > 0),
            Err(e) => assert!(e.contains("rlimit"), "got: {e}"),
        }
    }
}
