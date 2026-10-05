//! Host architecture, including an Intel process running under Rosetta.
pub fn native_arch() -> &'static str {
    #[cfg(target_os = "macos")]
    if std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "hw.optional.arm64"])
        .output()
        .is_ok_and(|out| out.status.success() && out.stdout.trim_ascii() == b"1")
    {
        return "aarch64";
    }
    std::env::consts::ARCH
}
