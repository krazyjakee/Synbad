//! Synbad configuration: the single source of truth for screen layout and
//! Core process settings. Synbad reads/writes this as TOML and generates the
//! Synergy Core `.conf` from it. The Core `.conf` is a build artifact — never
//! hand-edited at runtime.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub mod paths;

/// Top-level configuration model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Role this machine plays: server (input source) or client.
    pub role: NodeRole,
    /// Name of the screen acting as the server (must match one of `screens`).
    pub server_name: String,
    /// For client role: address of the server (`host` or `host:port`).
    #[serde(default)]
    pub server_address: Option<String>,
    /// TCP port the server listens on (server role) — Synergy default 24800.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Synbad daemon's own TCP port for the pairing handshake. Separate
    /// from the Core port because the protocols are independent and we
    /// want both daemons to be addressable when running on the same
    /// machine (e.g. for testing).
    #[serde(default = "default_service_port")]
    pub service_port: u16,
    /// Synbad daemon's TCP port for LAN config-sync sessions. Separate
    /// from `service_port` so the sync listener has an independent
    /// lifecycle and so a flood of sync traffic can't starve the pairing
    /// listener. Defaults to `service_port + 1`.
    #[serde(default = "default_sync_port")]
    pub sync_port: u16,
    /// All screens participating in the layout.
    #[serde(default)]
    pub screens: Vec<Screen>,
    /// Adjacencies between screens.
    #[serde(default)]
    pub links: Vec<Link>,
    /// Free-form Core options (rendered into `section: options`).
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    /// Where to find the Synergy Core executables.
    #[serde(default)]
    pub binaries: BinaryPaths,
}

fn default_port() -> u16 {
    24800
}

fn default_service_port() -> u16 {
    24850
}

fn default_sync_port() -> u16 {
    24851
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NodeRole {
    Server,
    Client,
}

/// A participating machine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Screen {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Grid placement for the GUI layout editor. Logical pixels.
    #[serde(default)]
    pub position: GridPosition,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GridPosition {
    pub x: i32,
    pub y: i32,
    #[serde(default = "default_screen_size")]
    pub w: u32,
    #[serde(default = "default_screen_size")]
    pub h: u32,
}

fn default_screen_size() -> u32 {
    160
}

/// A directional adjacency: when the cursor leaves `from` on `side`, it
/// arrives on `to`. The Synergy Core also requires the reverse direction;
/// `Config::generate_synergy_conf` emits both.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Link {
    pub from: String,
    pub side: Side,
    pub to: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
    Up,
    Down,
}

impl Side {
    pub fn opposite(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
            Side::Up => Side::Down,
            Side::Down => Side::Up,
        }
    }

    fn as_synergy(self) -> &'static str {
        match self {
            Side::Left => "left",
            Side::Right => "right",
            Side::Up => "up",
            Side::Down => "down",
        }
    }
}

/// Optional override for the Deskflow Core executable path. If `None`,
/// `synbadd` fetches the pinned `deskflow-core` from upstream on first
/// start (and, for older pinned releases, falls back to split
/// `deskflow-server` / `deskflow-client` binaries automatically). An
/// override is always treated as a unified `deskflow-core`: the supervisor
/// invokes it as `<path> server|client -s <settings.ini>`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BinaryPaths {
    pub core: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            role: NodeRole::Server,
            server_name: "this-machine".into(),
            server_address: None,
            port: default_port(),
            service_port: default_service_port(),
            sync_port: default_sync_port(),
            screens: vec![Screen {
                name: "this-machine".into(),
                aliases: vec![],
                position: GridPosition {
                    x: 0,
                    y: 0,
                    w: 160,
                    h: 120,
                },
            }],
            links: vec![],
            options: BTreeMap::new(),
            binaries: BinaryPaths::default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("toml deserialize: {0}")]
    TomlDe(#[from] toml::de::Error),
    #[error("toml serialize: {0}")]
    TomlSer(#[from] toml::ser::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
}

impl Config {
    /// Load config from a TOML file. If the file doesn't exist, returns
    /// `Ok(None)` so the caller can decide whether to seed a default.
    pub fn load(path: &Path) -> Result<Option<Self>, Error> {
        match fs::read_to_string(path) {
            Ok(s) => {
                let cfg: Config = toml::from_str(&s)?;
                cfg.validate()?;
                Ok(Some(cfg))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(Error::Io {
                path: path.into(),
                source,
            }),
        }
    }

    /// Save config atomically (write to temp, fsync, rename).
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        self.validate()?;
        let body = toml::to_string_pretty(self)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| Error::Io {
                path: parent.into(),
                source,
            })?;
        }
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, body).map_err(|source| Error::Io {
            path: tmp.clone(),
            source,
        })?;
        fs::rename(&tmp, path).map_err(|source| Error::Io {
            path: path.into(),
            source,
        })?;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), Error> {
        if self.screens.iter().all(|s| s.name != self.server_name) {
            return Err(Error::Invalid(format!(
                "server_name {:?} does not match any screen",
                self.server_name
            )));
        }
        for l in &self.links {
            if !self.screens.iter().any(|s| s.name == l.from) {
                return Err(Error::Invalid(format!(
                    "link.from {:?} is not a screen",
                    l.from
                )));
            }
            if !self.screens.iter().any(|s| s.name == l.to) {
                return Err(Error::Invalid(format!(
                    "link.to {:?} is not a screen",
                    l.to
                )));
            }
            if l.from == l.to {
                return Err(Error::Invalid(format!("link self-loop on {:?}", l.from)));
            }
        }
        if matches!(self.role, NodeRole::Client) && self.server_address.is_none() {
            return Err(Error::Invalid(
                "client role requires `server_address`".into(),
            ));
        }
        Ok(())
    }

    /// Does `other` differ from `self` in any field the Synergy/Deskflow
    /// Core actually consumes?
    ///
    /// The Core only ever sees what flows into `generate_synergy_conf` /
    /// `generate_deskflow_settings` plus which binary we launch. The
    /// daemon-only ports — `service_port` (pairing) and `sync_port`
    /// (config sync) — drive mDNS/pairing/sync and never reach the Core,
    /// so an edit that touches only those should apply silently instead
    /// of bouncing active input sharing. Everything else (role,
    /// server_name, server_address, port, screens, links, options,
    /// binary override) is a Core input.
    pub fn core_inputs_differ(&self, other: &Config) -> bool {
        let core_view = |c: &Config| {
            let mut c = c.clone();
            c.service_port = 0;
            c.sync_port = 0;
            c
        };
        core_view(self) != core_view(other)
    }

    /// Render this config as a Synergy Core `.conf` file body. Always emits
    /// both directions of every link (Synergy requires explicit reverse
    /// edges) and adds a generated-file banner.
    pub fn generate_synergy_conf(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "# Generated by Synbad. DO NOT EDIT — regenerated from config.toml."
        );
        let _ = writeln!(out);

        let _ = writeln!(out, "section: screens");
        for s in &self.screens {
            let _ = writeln!(out, "\t{}:", s.name);
        }
        let _ = writeln!(out, "end");
        let _ = writeln!(out);

        let has_aliases = self.screens.iter().any(|s| !s.aliases.is_empty());
        if has_aliases {
            let _ = writeln!(out, "section: aliases");
            for s in &self.screens {
                if s.aliases.is_empty() {
                    continue;
                }
                let _ = writeln!(out, "\t{}:", s.name);
                for a in &s.aliases {
                    let _ = writeln!(out, "\t\t{}", a);
                }
            }
            let _ = writeln!(out, "end");
            let _ = writeln!(out);
        }

        if !self.links.is_empty() {
            // Group all outgoing edges by `from`, including the implicit
            // reverse of each user-supplied link.
            let mut by_from: BTreeMap<&str, Vec<(Side, &str)>> = BTreeMap::new();
            for l in &self.links {
                by_from.entry(&l.from).or_default().push((l.side, &l.to));
                by_from
                    .entry(&l.to)
                    .or_default()
                    .push((l.side.opposite(), &l.from));
            }
            let _ = writeln!(out, "section: links");
            for (from, edges) in by_from {
                let _ = writeln!(out, "\t{}:", from);
                for (side, to) in edges {
                    let _ = writeln!(out, "\t\t{}(0,100) = {}(0,100)", side.as_synergy(), to);
                }
            }
            let _ = writeln!(out, "end");
            let _ = writeln!(out);
        }

        if !self.options.is_empty() {
            let _ = writeln!(out, "section: options");
            for (k, v) in &self.options {
                let _ = writeln!(out, "\t{} = {}", k, v);
            }
            let _ = writeln!(out, "end");
        }

        out
    }

    /// Render the Deskflow QSettings INI that the modern `deskflow-core`
    /// binary reads via its `-s settings.ini` flag.
    ///
    /// Keys mirror `Deskflow/src/lib/common/Settings.h`:
    ///   * `core/coreMode` (1 = Client, 2 = Server)
    ///   * `core/computerName`
    ///   * `core/port`
    ///   * `core/processMode` (1 = Desktop — no service elevation)
    ///   * `client/remoteHost` (client mode only)
    ///   * `server/externalConfig` + `server/externalConfigFile` (server mode):
    ///     points Deskflow at the screen-layout `.conf` produced by
    ///     `generate_synergy_conf` so layouts authored in Synbad's GUI flow
    ///     through unchanged.
    pub fn generate_deskflow_settings(&self, screen_conf_path: &Path) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "; Generated by Synbad. DO NOT EDIT — regenerated from config.toml."
        );
        let _ = writeln!(out);

        let _ = writeln!(out, "[core]");
        let core_mode = match self.role {
            NodeRole::Client => 1,
            NodeRole::Server => 2,
        };
        let _ = writeln!(out, "coreMode={}", core_mode);
        let _ = writeln!(out, "computerName={}", self.server_name);
        let _ = writeln!(out, "port={}", self.port);
        // 1 = Desktop. Service mode (0) would require root and a system-wide install.
        let _ = writeln!(out, "processMode=1");
        let _ = writeln!(out);

        match self.role {
            NodeRole::Client => {
                let _ = writeln!(out, "[client]");
                if let Some(addr) = &self.server_address {
                    let addr = if addr.contains(':') {
                        addr.clone()
                    } else {
                        format!("{}:{}", addr, self.port)
                    };
                    let _ = writeln!(out, "remoteHost={}", addr);
                }
            }
            NodeRole::Server => {
                let _ = writeln!(out, "[server]");
                let _ = writeln!(out, "externalConfig=true");
                let _ = writeln!(
                    out,
                    "externalConfigFile={}",
                    screen_conf_path.to_string_lossy()
                );
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        Config {
            role: NodeRole::Server,
            server_name: "alpha".into(),
            server_address: None,
            port: 24800,
            service_port: 24850,
            sync_port: 24851,
            screens: vec![
                Screen {
                    name: "alpha".into(),
                    aliases: vec!["alpha.local".into()],
                    position: GridPosition {
                        x: 0,
                        y: 0,
                        w: 160,
                        h: 120,
                    },
                },
                Screen {
                    name: "beta".into(),
                    aliases: vec![],
                    position: GridPosition {
                        x: 160,
                        y: 0,
                        w: 160,
                        h: 120,
                    },
                },
            ],
            links: vec![Link {
                from: "alpha".into(),
                side: Side::Right,
                to: "beta".into(),
            }],
            options: BTreeMap::from([("heartbeat".to_string(), "5000".to_string())]),
            binaries: BinaryPaths::default(),
        }
    }

    #[test]
    fn roundtrips_toml() {
        let c = sample();
        let s = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn generates_links_in_both_directions() {
        let conf = sample().generate_synergy_conf();
        assert!(conf.contains("right(0,100) = beta(0,100)"));
        assert!(conf.contains("left(0,100) = alpha(0,100)"));
    }

    #[test]
    fn generates_aliases_section_only_when_needed() {
        let mut c = sample();
        assert!(c.generate_synergy_conf().contains("section: aliases"));
        for s in &mut c.screens {
            s.aliases.clear();
        }
        assert!(!c.generate_synergy_conf().contains("section: aliases"));
    }

    #[test]
    fn validate_rejects_unknown_screen_in_link() {
        let mut c = sample();
        c.links[0].to = "ghost".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_requires_server_address_for_client() {
        let mut c = sample();
        c.role = NodeRole::Client;
        assert!(c.validate().is_err());
        c.server_address = Some("alpha.local:24800".into());
        assert!(c.validate().is_ok());
    }

    #[test]
    fn core_inputs_differ_ignores_daemon_only_ports() {
        let a = sample();
        let mut b = a.clone();
        b.service_port = a.service_port + 1;
        b.sync_port = a.sync_port + 1;
        assert!(
            !a.core_inputs_differ(&b),
            "pairing/sync port tweaks must not count as Core input changes"
        );

        b.port = a.port + 1;
        assert!(a.core_inputs_differ(&b), "Core port is a Core input");

        let mut c = a.clone();
        c.role = NodeRole::Client;
        c.server_address = Some("peer.local:24800".into());
        assert!(a.core_inputs_differ(&c), "role change is a Core input");
    }

    #[test]
    fn server_settings_ini_references_screen_conf() {
        let c = sample();
        let ini = c.generate_deskflow_settings(Path::new("/tmp/x.conf"));
        assert!(ini.contains("coreMode=2"));
        assert!(ini.contains("computerName=alpha"));
        assert!(ini.contains("port=24800"));
        assert!(ini.contains("externalConfig=true"));
        assert!(ini.contains("externalConfigFile=/tmp/x.conf"));
    }

    #[test]
    fn client_settings_ini_has_remote_host() {
        let mut c = sample();
        c.role = NodeRole::Client;
        c.server_address = Some("alpha.local".into());
        let ini = c.generate_deskflow_settings(Path::new("/tmp/x.conf"));
        assert!(ini.contains("coreMode=1"));
        // Port appended when missing.
        assert!(ini.contains("remoteHost=alpha.local:24800"));
        assert!(!ini.contains("externalConfig=true"));
    }
}
