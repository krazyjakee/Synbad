//! Re-launch the privileged half of the updater under elevated rights when
//! the install location isn't writable by the running user.
//!
//! On Linux this is `pkexec` (PolicyKit, gives a graphical auth dialog under
//! a desktop session) with a `sudo -n …` fallback only used when an explicit
//! tty is attached and pkexec is missing — `sudo` without `-n` would block
//! the GUI worker thread on a password prompt at an invisible terminal.
//! macOS uses `osascript` with administrator privileges (the system auth
//! prompt). Windows uses the `runas` ShellExecute verb to trigger UAC.
//!
//! The helper is always a sibling Synbad binary on disk, invoked with the
//! hidden subcommand `__apply-update --plan <path>`. The plan describes a
//! list of file moves the helper must perform as root.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Run `helper __apply-update --plan plan_path` under elevated privileges.
///
/// `helper` is the path to the privileged worker binary — typically the
/// sibling `synbadd` daemon alongside the running `synbad-gui`. Returns Ok
/// when the helper exits 0, otherwise an error describing the
/// platform-specific failure (auth cancelled, helper non-zero exit, missing
/// elevation tool).
pub(crate) fn apply_with_elevation(helper: &Path, plan_path: &Path) -> Result<()> {
    if !helper.is_file() {
        bail!(
            "elevation helper not found at `{}` — the sibling Synbad binary must \
             be installed alongside the running one for in-place updates to \
             system locations",
            helper.display()
        );
    }
    #[cfg(target_os = "linux")]
    {
        return linux::run(helper, plan_path);
    }
    #[cfg(target_os = "macos")]
    {
        return macos::run(helper, plan_path);
    }
    #[cfg(target_os = "windows")]
    {
        return windows::run(helper, plan_path);
    }
    #[allow(unreachable_code)]
    {
        let _ = (helper, plan_path);
        bail!("automatic elevation is not implemented for this platform")
    }
}

/// Where to look for the elevation helper given the path to the running
/// binary. Prefers the sibling `synbadd` daemon (which has no UI side
/// effects when relaunched briefly) when the running binary is the GUI;
/// otherwise returns the running binary itself (re-launching ourselves
/// under elevation is fine — the early arg check in main exits before any
/// runtime setup).
pub(crate) fn helper_path_for(current_exe: &Path) -> PathBuf {
    let parent = current_exe.parent().unwrap_or_else(|| Path::new(""));
    let name = current_exe
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let stem = name.strip_suffix(".exe").unwrap_or(name);
    if stem == "synbadd" {
        return current_exe.to_path_buf();
    }
    let helper_name = if cfg!(windows) {
        "synbadd.exe"
    } else {
        "synbadd"
    };
    let candidate = parent.join(helper_name);
    if candidate.is_file() {
        candidate
    } else {
        // No sibling daemon on disk — fall back to re-launching ourselves.
        // The GUI's main() handles `__apply-update` before initializing
        // eframe, so this is safe.
        current_exe.to_path_buf()
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    pub(super) fn run(helper: &Path, plan_path: &Path) -> Result<()> {
        // Prefer pkexec when we are running under a graphical session — it
        // pops a polkit auth dialog the user can answer without a terminal.
        // The DISPLAY/WAYLAND_DISPLAY check keeps us from invoking pkexec on
        // a headless session where it would block forever.
        let has_display =
            std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
        let pkexec_ok = which("pkexec").is_some();
        if has_display && pkexec_ok {
            // pkexec wipes most of the environment; that's fine, the helper
            // just performs file moves and reads only the plan path.
            let status = Command::new("pkexec")
                .arg(helper)
                .arg("__apply-update")
                .arg("--plan")
                .arg(plan_path)
                .status()
                .with_context(|| "spawn pkexec")?;
            return interpret_status(status, "pkexec");
        }
        // Tty fallback: invoke the system installed `sudo`, which prompts on
        // the controlling terminal. Only used when we know there is a tty
        // attached, so we don't deadlock on an invisible password prompt.
        if attached_to_tty() && which("sudo").is_some() {
            let status = Command::new("sudo")
                .arg(helper)
                .arg("__apply-update")
                .arg("--plan")
                .arg(plan_path)
                .status()
                .with_context(|| "spawn sudo")?;
            return interpret_status(status, "sudo");
        }
        bail!(
            "no elevation tool available — install `pkexec` (preferred for GUI \
             sessions) or run the updater from a terminal with `sudo`"
        )
    }

    fn attached_to_tty() -> bool {
        // SAFETY: isatty is a thread-safe libc call.
        unsafe { libc_isatty(0) }
    }

    // libc isatty without pulling the libc crate in just for this. We declare
    // the prototype directly; the symbol is part of every libc on Linux.
    extern "C" {
        fn isatty(fd: std::os::raw::c_int) -> std::os::raw::c_int;
    }
    unsafe fn libc_isatty(fd: i32) -> bool {
        isatty(fd) == 1
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;

    pub(super) fn run(helper: &Path, plan_path: &Path) -> Result<()> {
        // osascript's "with administrator privileges" pops the standard macOS
        // auth sheet. We escape paths defensively for the AppleScript string.
        let quoted_helper = shell_escape(&helper.display().to_string());
        let quoted_plan = shell_escape(&plan_path.display().to_string());
        let script = format!(
            "do shell script \"{} __apply-update --plan {}\" with administrator privileges",
            quoted_helper, quoted_plan,
        );
        let status = Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .status()
            .with_context(|| "spawn osascript")?;
        interpret_status(status, "osascript")
    }

    fn shell_escape(s: &str) -> String {
        s.replace('\\', "\\\\").replace('"', "\\\"")
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use super::*;
    use std::ffi::OsStr;
    use std::iter::once;
    use std::os::windows::ffi::OsStrExt;

    pub(super) fn run(helper: &Path, plan_path: &Path) -> Result<()> {
        // ShellExecuteW with the "runas" verb triggers the UAC consent
        // prompt. The call returns once the new process is spawned, so we
        // need to wait on the resulting process handle ourselves.
        let exe_w = wide(helper.as_os_str());
        let params = format!("__apply-update --plan \"{}\"", plan_path.display());
        let params_w = wide(OsStr::new(&params));
        let verb_w = wide(OsStr::new("runas"));

        unsafe {
            let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
            info.cb_size = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
            info.f_mask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
            info.lp_verb = verb_w.as_ptr();
            info.lp_file = exe_w.as_ptr();
            info.lp_parameters = params_w.as_ptr();
            info.n_show = SW_HIDE;

            if ShellExecuteExW(&mut info) == 0 {
                let err = GetLastError();
                if err == ERROR_CANCELLED {
                    bail!("update cancelled — administrator approval was declined");
                }
                bail!("ShellExecuteExW(runas) failed (code {err})");
            }
            if info.h_process.is_null() {
                bail!("elevated helper did not return a process handle");
            }
            WaitForSingleObject(info.h_process, INFINITE);
            let mut exit: u32 = 1;
            GetExitCodeProcess(info.h_process, &mut exit);
            CloseHandle(info.h_process);
            if exit != 0 {
                bail!("elevated helper exited with status {exit}");
            }
            Ok(())
        }
    }

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(once(0)).collect()
    }

    // Minimal Win32 bindings — we only need a handful of symbols and don't
    // want to depend on the full `windows` crate just for the updater.
    type HANDLE = *mut std::ffi::c_void;
    const INFINITE: u32 = 0xFFFFFFFF;
    const SW_HIDE: i32 = 0;
    const SEE_MASK_NOCLOSEPROCESS: u32 = 0x00000040;
    const SEE_MASK_NOASYNC: u32 = 0x00000100;
    const ERROR_CANCELLED: u32 = 1223;

    #[repr(C)]
    struct SHELLEXECUTEINFOW {
        cb_size: u32,
        f_mask: u32,
        h_wnd: HANDLE,
        lp_verb: *const u16,
        lp_file: *const u16,
        lp_parameters: *const u16,
        lp_directory: *const u16,
        n_show: i32,
        h_inst_app: HANDLE,
        lp_id_list: *mut std::ffi::c_void,
        lp_class: *const u16,
        h_key_class: HANDLE,
        dw_hot_key: u32,
        h_icon_or_monitor: HANDLE,
        h_process: HANDLE,
    }

    extern "system" {
        fn ShellExecuteExW(info: *mut SHELLEXECUTEINFOW) -> i32;
        fn WaitForSingleObject(h: HANDLE, ms: u32) -> u32;
        fn GetExitCodeProcess(h: HANDLE, code: *mut u32) -> i32;
        fn CloseHandle(h: HANDLE) -> i32;
        fn GetLastError() -> u32;
    }
}

/// Shared helper: turn a child `ExitStatus` into `Ok(())` / a typed error.
/// Distinguishes "user cancelled the auth prompt" from "helper failed at
/// runtime" so the UI can show the right copy.
#[allow(dead_code)]
fn interpret_status(status: std::process::ExitStatus, tool: &str) -> Result<()> {
    if status.success() {
        return Ok(());
    }
    // pkexec exits 126 for "auth dialog cancelled", 127 for "auth failed".
    // sudo exits 1 when the user declines / mistypes the password.
    match status.code() {
        Some(126) => bail!("{tool}: authentication dialog was cancelled"),
        Some(127) => bail!("{tool}: authentication failed"),
        Some(c) => bail!("{tool}: helper exited with status {c}"),
        None => bail!("{tool}: helper terminated by signal"),
    }
}

#[cfg(target_os = "linux")]
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_path_for_daemon_returns_self() {
        let p = helper_path_for(Path::new("/usr/bin/synbadd"));
        assert_eq!(p, PathBuf::from("/usr/bin/synbadd"));
    }

    #[test]
    fn helper_path_for_gui_with_no_sibling_falls_back_to_self() {
        // /tmp/nonexistent-X has no synbadd sibling on disk, so we must
        // fall back to the GUI itself (it handles __apply-update in main).
        let gui = Path::new("/tmp/no-such-dir-synbad/synbad-gui");
        let p = helper_path_for(gui);
        assert_eq!(p, gui.to_path_buf());
    }
}
