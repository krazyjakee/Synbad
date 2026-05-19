//! XDG-aware paths for the user-level Synbad install.

use std::path::PathBuf;

use directories::ProjectDirs;

fn dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("dev", "synbad", "synbad")
}

/// `~/.config/synbad/` (Linux) / `~/Library/Application Support/synbad/synbad/` (macOS).
pub fn config_dir() -> PathBuf {
    dirs()
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("./synbad-config"))
}

/// `~/.local/state/synbad/` — runtime artifacts (generated `.conf`, sockets).
pub fn state_dir() -> PathBuf {
    dirs()
        .map(|d| d.data_local_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("./synbad-state"))
}

/// The Synbad config TOML.
pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

/// Sidecar JSON holding the per-field Lamport stamps + last-seen clock
/// for the config. Lives next to `config.toml` so a backup of the config
/// dir captures both pieces. Missing/old sidecars are tolerated: a fresh
/// one is materialised at the next local edit.
pub fn config_versions_file() -> PathBuf {
    config_dir().join("config.versions.json")
}

/// The generated Synergy/Deskflow screen-layout `.conf` (build artifact).
/// Referenced from the Deskflow settings INI as `server/externalConfigFile`.
pub fn generated_conf() -> PathBuf {
    state_dir().join("synergy.conf")
}

/// The generated Deskflow QSettings INI passed to `deskflow-core -s`.
pub fn generated_settings() -> PathBuf {
    state_dir().join("deskflow.ini")
}

/// The local IPC endpoint the daemon listens on. Per-user.
///
/// On Unix this is a filesystem path under [`state_dir`]. On Windows this is
/// a named-pipe path of the form `\\.\pipe\synbadd-<user>`. Either form is
/// accepted by `interprocess::local_socket` as a `GenericFilePath` name.
pub fn ipc_socket() -> PathBuf {
    #[cfg(windows)]
    {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "anon".into());
        // Sanitize: pipe names can't contain backslashes, and we keep this short.
        let safe: String = user
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        PathBuf::from(format!(r"\\.\pipe\synbadd-{}", safe))
    }
    #[cfg(not(windows))]
    {
        let uid = std::env::var("UID")
            .ok()
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_else(|| "anon".into());
        state_dir().join(format!("synbadd-{}.sock", uid))
    }
}
