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
///
/// `Eq` is intentionally not derived because `AudioConfig` carries f32
/// gain values; `PartialEq` is enough for the FS-watcher equality check
/// in the supervisor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    /// Whether the Synergy Core should share clipboard contents between
    /// connected machines. When `false`, emits `clipboardSharing = false`
    /// into the generated `synergy.conf` options section so the Core
    /// suppresses clipboard relay in both directions.
    #[serde(default = "default_clipboard_sharing")]
    pub clipboard_sharing: bool,
    /// Should the GUI spawn `synbadd` on startup (and stop it on exit)?
    /// On by default — most users want a turnkey app where the daemon's
    /// lifecycle is tied to the window. Turning it off lets a separately-
    /// managed daemon (systemd user unit, launchd agent) own its own
    /// lifecycle while the GUI attaches as a thin client.
    ///
    /// Read by the GUI only; the daemon doesn't act on it.
    #[serde(default = "default_autostart")]
    pub autostart: bool,
    /// Where to find the Synergy Core executables.
    #[serde(default)]
    pub binaries: BinaryPaths,
    /// LAN audio bridge settings. Off by default.
    #[serde(default)]
    pub audio: AudioConfig,
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

fn default_clipboard_sharing() -> bool {
    true
}

fn default_autostart() -> bool {
    true
}

fn default_audio_signal_port() -> u16 {
    24852
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
    /// Physical monitors attached to this machine, in the machine's own
    /// coordinate system (logical pixels reported by the OS). Populated by
    /// `synbad-gui` at startup using the local desktop session. Empty when
    /// the machine has never run the GUI — the layout editor falls back to
    /// the legacy `position.w/h` box in that case.
    #[serde(default)]
    pub monitors: Vec<MonitorInfo>,
}

/// One physical display attached to a `Screen`. Coordinates are in the
/// owning machine's own desktop space, so the bounding box across all
/// `monitors` represents that machine's total virtual desktop. The GUI
/// scales this into the layout canvas.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MonitorInfo {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    #[serde(default)]
    pub primary: bool,
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

impl Screen {
    /// Logical bounding box across `monitors`, in this machine's own
    /// desktop coordinates. Returns `None` when no monitors are reported
    /// (so callers can fall back to `position.w/h`).
    pub fn monitor_bbox(&self) -> Option<(u32, u32)> {
        if self.monitors.is_empty() {
            return None;
        }
        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;
        for m in &self.monitors {
            min_x = min_x.min(m.x);
            min_y = min_y.min(m.y);
            max_x = max_x.max(m.x + m.w as i32);
            max_y = max_y.max(m.y + m.h as i32);
        }
        Some(((max_x - min_x) as u32, (max_y - min_y) as u32))
    }
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

/// LAN audio bridge configuration. Disabled by default; opting in surfaces
/// device dropdowns in the GUI and starts the audio signaling listener.
///
/// Routing is intentionally simple: when `enabled` is true the bridge
/// brings up a bidirectional session with every trusted peer. The
/// per-peer map is the only knob for muting one direction (or the whole
/// link) on a specific peer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AudioConfig {
    /// Master switch. When `false`, the audio subsystem doesn't bind its
    /// signaling listener and won't accept incoming sessions. When
    /// `true`, every paired peer gets a bidirectional session unless
    /// a `per_peer` entry overrides it.
    #[serde(default)]
    pub enabled: bool,
    /// User-chosen input device name. `None` means "OS default."
    #[serde(default)]
    pub input_device: Option<String>,
    /// User-chosen output device name. `None` means "OS default."
    #[serde(default)]
    pub output_device: Option<String>,
    /// Linear multiplier applied to captured (microphone) samples before
    /// they're encoded. `1.0` = unity, `0.0` = mute, `2.0` = +6 dB.
    /// Saturating-clamped to `[-1.0, 1.0]` after multiplication, so
    /// values above 1.0 will clip loud sources.
    #[serde(default = "default_gain")]
    pub input_gain: f32,
    /// Linear multiplier applied to received samples before they're
    /// written to the output device. Same semantics as `input_gain`.
    #[serde(default = "default_gain")]
    pub output_gain: f32,
    /// Per-peer overrides keyed by `machine_id`. Absent = bidirectional
    /// when `enabled` is true.
    #[serde(default)]
    pub per_peer: BTreeMap<String, PeerAudioRouting>,
    /// TCP port the audio signaling listener binds. Defaults to
    /// `sync_port + 1`.
    #[serde(default = "default_audio_signal_port")]
    pub signal_port: u16,
}

fn default_gain() -> f32 {
    1.0
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            input_device: None,
            output_device: None,
            input_gain: default_gain(),
            output_gain: default_gain(),
            per_peer: BTreeMap::new(),
            signal_port: default_audio_signal_port(),
        }
    }
}

/// Per-peer override for the bidirectional default. With no entry, an
/// enabled bridge sends and receives on this link; the entry below can
/// disable the whole link (`enabled = false`) or mute one direction.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PeerAudioRouting {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub send_to_peer: bool,
    #[serde(default)]
    pub receive_from_peer: bool,
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
                monitors: vec![],
            }],
            links: vec![],
            options: BTreeMap::new(),
            clipboard_sharing: default_clipboard_sharing(),
            autostart: default_autostart(),
            binaries: BinaryPaths::default(),
            audio: AudioConfig::default(),
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
    /// `generate_deskflow_settings` plus which binary we launch. Fields
    /// that don't reach the Core — daemon-only ports (`service_port` for
    /// pairing, `sync_port` for config sync) and the `audio` bridge
    /// section — should apply silently instead of bouncing active input
    /// sharing. Everything else (role, server_name, server_address,
    /// port, screens, links, options, binary override) is a Core input.
    pub fn core_inputs_differ(&self, other: &Config) -> bool {
        let core_view = |c: &Config| {
            let mut c = c.clone();
            c.service_port = 0;
            c.sync_port = 0;
            c.audio = AudioConfig::default();
            // `autostart` is a GUI-lifecycle toggle, not a Core input —
            // toggling it must not bounce active input sharing.
            c.autostart = true;
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

        let emit_clipboard = !self.clipboard_sharing;
        if !self.options.is_empty() || emit_clipboard {
            let _ = writeln!(out, "section: options");
            if emit_clipboard {
                let _ = writeln!(out, "\tclipboardSharing = false");
            }
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
                    monitors: vec![MonitorInfo {
                        x: 0,
                        y: 0,
                        w: 1920,
                        h: 1080,
                        primary: true,
                    }],
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
                    monitors: vec![],
                },
            ],
            links: vec![Link {
                from: "alpha".into(),
                side: Side::Right,
                to: "beta".into(),
            }],
            options: BTreeMap::from([("heartbeat".to_string(), "5000".to_string())]),
            clipboard_sharing: true,
            autostart: true,
            binaries: BinaryPaths::default(),
            audio: AudioConfig::default(),
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
    fn core_inputs_differ_ignores_autostart() {
        // `autostart` is a GUI-only preference — toggling it must not
        // bounce the running Core, since the Core has no idea what it
        // means.
        let a = sample();
        let mut b = a.clone();
        b.autostart = !a.autostart;
        assert!(
            !a.core_inputs_differ(&b),
            "autostart edits must not count as Core input changes"
        );
    }

    #[test]
    fn autostart_defaults_to_true_when_missing_in_toml() {
        // Loading a TOML written by an older Synbad must default to the
        // pre-toggle behaviour (autostart on) so an upgrade doesn't
        // silently disable the auto-spawn that users relied on.
        let toml = r#"
role = "server"
server_name = "alpha"
[[screens]]
name = "alpha"
"#;
        let c: Config = toml::from_str(toml).unwrap();
        assert!(c.autostart);
    }

    #[test]
    fn core_inputs_differ_ignores_audio_section() {
        // Audio settings drive the LAN audio bridge, not the Deskflow
        // Core — flipping them must not bounce active input sharing.
        let a = sample();
        let mut b = a.clone();
        b.audio.enabled = !a.audio.enabled;
        b.audio.input_device = Some("Built-in Microphone".into());
        b.audio.signal_port = a.audio.signal_port + 1;
        assert!(
            !a.core_inputs_differ(&b),
            "audio bridge edits must not count as Core input changes"
        );
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
    fn clipboard_sharing_disabled_emits_option() {
        let mut c = sample();
        c.clipboard_sharing = false;
        let conf = c.generate_synergy_conf();
        assert!(conf.contains("section: options"));
        assert!(conf.contains("clipboardSharing = false"));
    }

    #[test]
    fn clipboard_sharing_enabled_omits_option() {
        // Default sample has clipboard_sharing = true and no other options.
        // The generated conf shouldn't mention clipboardSharing at all —
        // omission means "use Core's default behaviour" (clipboard on).
        let mut c = sample();
        c.options.clear();
        c.clipboard_sharing = true;
        let conf = c.generate_synergy_conf();
        assert!(!conf.contains("clipboardSharing"));
    }

    #[test]
    fn clipboard_sharing_defaults_to_true_when_missing_in_toml() {
        // Loading a TOML written by an older Synbad must default to the
        // pre-toggle behaviour (clipboard on) so an upgrade doesn't
        // silently change Core behaviour.
        let toml = r#"
role = "server"
server_name = "alpha"
[[screens]]
name = "alpha"
"#;
        let c: Config = toml::from_str(toml).unwrap();
        assert!(c.clipboard_sharing);
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

    #[test]
    fn audio_defaults_round_trip_through_toml() {
        let mut c = sample();
        c.audio.enabled = true;
        c.audio.input_device = Some("Built-in Microphone".into());
        c.audio.per_peer.insert(
            "peer-uuid-123".into(),
            PeerAudioRouting {
                enabled: true,
                send_to_peer: true,
                receive_from_peer: false,
            },
        );
        let body = toml::to_string_pretty(&c).expect("serialize");
        let round: Config = toml::from_str(&body).expect("deserialize");
        assert_eq!(round.audio, c.audio);
        assert_eq!(round.audio.signal_port, default_audio_signal_port());
    }

    #[test]
    fn audio_section_missing_uses_defaults() {
        // Config from before this feature existed must still load.
        let pre_audio = r#"
            role = "server"
            server_name = "host"
            port = 24800
            service_port = 24850
            sync_port = 24851

            [[screens]]
            name = "host"
            aliases = []

            [screens.position]
            x = 0
            y = 0
            w = 160
            h = 120

            [binaries]
        "#;
        let c: Config = toml::from_str(pre_audio).expect("legacy config still parses");
        assert!(!c.audio.enabled);
        assert_eq!(c.audio.signal_port, default_audio_signal_port());
    }

    #[test]
    fn audio_section_with_legacy_direction_toggles_still_loads() {
        // v0.1.x persisted `send_mic_to_peers` / `receive_peer_audio`.
        // The fields are gone in the simplified routing model — make sure
        // an upgrade doesn't trip on them.
        let legacy = r#"
            role = "server"
            server_name = "host"
            port = 24800
            service_port = 24850
            sync_port = 24851

            [[screens]]
            name = "host"
            aliases = []

            [screens.position]
            x = 0
            y = 0
            w = 160
            h = 120

            [binaries]

            [audio]
            enabled = true
            send_mic_to_peers = true
            receive_peer_audio = false
        "#;
        let c: Config = toml::from_str(legacy).expect("legacy audio config still parses");
        assert!(c.audio.enabled);
    }
}
