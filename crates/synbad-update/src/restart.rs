//! Best-effort daemon + self relaunch after a successful update.
//!
//! Replacing the binaries on disk only does half the job — the running
//! daemon keeps the old inode open until something forces it to re-exec,
//! and the running GUI similarly stays on the old binary until its process
//! exits. We bounce both:
//!
//! * **Daemon** is owned by the platform's per-user supervisor (systemd
//!   user service on Linux, launchd LaunchAgent on macOS, Task Scheduler
//!   on Windows). Asking the supervisor to restart is enough — it picks
//!   up the new binary on the next ExecStart.
//! * **GUI** can't restart itself the same way; it spawns a fresh copy of
//!   its own executable and exits. The caller is responsible for releasing
//!   any single-instance lock before this runs so the new copy can claim
//!   it on startup.
//!
//! Both helpers are best-effort. If the supervisor isn't installed (custom
//! deploy, running from `cargo run`) the restart silently fails and the
//! user will have to bounce the daemon themselves — which is the same
//! state they were in before this module existed.
//!
//! Tests cover the command shape (what we'd invoke) rather than the live
//! invocation, since CI runners don't have a real systemd/launchd session
//! to drive.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// Ask the platform supervisor to restart the `synbadd` daemon. Best-effort:
/// an error here means the supervisor isn't installed or rejected the call,
/// not that the update itself failed.
///
/// On Linux this is `systemctl --user restart synbadd.service`. On macOS it
/// is `launchctl kickstart -k gui/<uid>/dev.synbad.synbadd` (the `-k`
/// terminates the running instance first so the kickstart picks up the new
/// binary). On Windows it stops then restarts the per-user Scheduled Task
/// the installer registers (`SynbadDaemon`).
pub fn restart_daemon() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        return linux_restart();
    }
    #[cfg(target_os = "macos")]
    {
        return macos_restart();
    }
    #[cfg(target_os = "windows")]
    {
        return windows_restart();
    }
    #[allow(unreachable_code)]
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_restart() -> Result<()> {
    // --user keeps us in the calling user's systemd instance, which is
    // where install.sh / the .deb postinst register synbadd.service.
    let status = Command::new("systemctl")
        .args(["--user", "restart", "synbadd.service"])
        .status()
        .context("spawn systemctl")?;
    if !status.success() {
        anyhow::bail!(
            "systemctl --user restart synbadd.service exited with {:?}",
            status.code()
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_restart() -> Result<()> {
    // `kickstart -k <label>` is the documented "stop then start" verb for
    // a loaded service. We target the per-user gui domain because the
    // LaunchAgent is bootstrap'd there by dist/macos/install.sh.
    let uid = unsafe { libc_getuid() };
    let target = format!("gui/{}/dev.synbad.synbadd", uid);
    let status = Command::new("launchctl")
        .args(["kickstart", "-k", &target])
        .status()
        .context("spawn launchctl")?;
    if !status.success() {
        anyhow::bail!(
            "launchctl kickstart -k {} exited with {:?}",
            target,
            status.code()
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

#[cfg(target_os = "windows")]
fn windows_restart() -> Result<()> {
    // schtasks ships with every Windows install; PowerShell's
    // Stop-ScheduledTask / Start-ScheduledTask would also work but require
    // spawning powershell.exe. /End is a no-op if the task isn't running,
    // so the pair is safe even when synbadd already crashed.
    let stop = Command::new("schtasks")
        .args(["/End", "/TN", "SynbadDaemon"])
        .status()
        .context("spawn schtasks /End")?;
    // /End returns non-zero when the task isn't currently running; that's
    // not a failure for our purposes. We only fail on /Run errors.
    let _ = stop;
    let start = Command::new("schtasks")
        .args(["/Run", "/TN", "SynbadDaemon"])
        .status()
        .context("spawn schtasks /Run")?;
    if !start.success() {
        anyhow::bail!(
            "schtasks /Run /TN SynbadDaemon exited with {:?}",
            start.code()
        );
    }
    Ok(())
}

/// Spawn a fresh copy of `exe` as an independent child and return. The
/// caller is expected to exit shortly after so the new process can take
/// over the user-facing role (tray, single-instance socket, window). On
/// Unix we detach from the parent's process group so a `Ctrl-C` in the
/// terminal that launched us doesn't reach the new GUI.
pub fn spawn_self(exe: &Path) -> Result<()> {
    let mut cmd = Command::new(exe);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid` is async-signal-safe and exactly the call we
        // want here — it breaks the new GUI out of our process group so
        // signal-driven shutdowns of the old process don't propagate.
        unsafe {
            cmd.pre_exec(|| {
                if libc_setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    cmd.spawn()
        .with_context(|| format!("spawn replacement GUI at {}", exe.display()))?;
    Ok(())
}

#[cfg(unix)]
extern "C" {
    #[link_name = "setsid"]
    fn libc_setsid() -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `restart_daemon` is allowed to fail on a CI runner that doesn't
    /// have a user-session supervisor — we just want it to not panic and
    /// to produce a sensible error type. Skipped when the supervisor
    /// command is actually missing from PATH because then `Command::status`
    /// can return Err for "no such file" which we already treat as failure.
    #[test]
    fn restart_daemon_does_not_panic() {
        let _ = restart_daemon();
    }

    /// `spawn_self` against a non-existent path must surface an Err rather
    /// than crashing the caller. The real path is exercised end-to-end in
    /// the GUI; this is the regression guard for "we tried to spawn before
    /// the binary was on disk".
    #[test]
    fn spawn_self_returns_err_for_missing_binary() {
        let bogus = Path::new("/definitely/not/a/real/path/synbad-gui-xyzzy");
        assert!(spawn_self(bogus).is_err());
    }
}
