//! Versioned config: `Config` + per-top-level-field Lamport timestamps.
//!
//! Each top-level field on [`Config`] carries its own [`LamportTime`]. A
//! merge is per-field Last-Write-Wins: whichever side has the higher
//! `(counter, machine_id)` stamp wins. Both sides bump their clock past
//! the merged-in clock so subsequent edits stay monotonically newer.
//!
//! ### Why per-field instead of per-document
//!
//! Per-document LWW would silently lose any concurrent edit that touched a
//! *different* field — A renaming the server while B added a screen ends
//! with one side's whole document overwritten. Per-field LWW preserves
//! both edits as long as they didn't touch the same field. It's the
//! cheapest model that's still useful in practice.
//!
//! ### What "field" means here
//!
//! Each top-level field of [`Config`] (`role`, `screens`, `links`,
//! `options`, ...) is treated as a single atomic unit. Two peers
//! concurrently editing *different* screens within the `screens` Vec will
//! still conflict and lose one side — that's a known v1 limitation,
//! tracked in CONFIG-SYNC.md. Config edits are infrequent enough that this
//! is acceptable until we move to a CRDT-style per-screen model.

use std::cmp::Ordering;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use synbad_config::Config;

#[derive(Debug, thiserror::Error)]
pub enum VersionedConfigError {
    #[error("io at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Lamport-style logical timestamp. Total order is `counter` then `origin`
/// (the originating peer's `machine_id`). Two peers can't legitimately
/// share both a counter and a machine_id, so the order is total and
/// deterministic — every peer breaks ties the same way.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
pub struct LamportTime {
    pub counter: u64,
    /// The peer's stable `machine_id` (UUID string from
    /// [`synbad_discovery::Identity`]). Empty only for the
    /// never-edited bootstrap stamp.
    pub origin: String,
}

impl Ord for LamportTime {
    fn cmp(&self, other: &Self) -> Ordering {
        self.counter
            .cmp(&other.counter)
            .then_with(|| self.origin.cmp(&other.origin))
    }
}

impl PartialOrd for LamportTime {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// One Lamport stamp per top-level [`Config`] field.
///
/// Adding a new field here requires bumping the on-disk schema; the
/// sidecar is `#[serde(default)]`-friendly so missing fields fall back to
/// `LamportTime::default()` (counter 0), guaranteeing any incoming peer
/// edit wins on first sync.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct FieldStamps {
    #[serde(default)]
    pub role: LamportTime,
    #[serde(default)]
    pub server_name: LamportTime,
    #[serde(default)]
    pub server_address: LamportTime,
    #[serde(default)]
    pub port: LamportTime,
    #[serde(default)]
    pub service_port: LamportTime,
    #[serde(default)]
    pub screens: LamportTime,
    #[serde(default)]
    pub links: LamportTime,
    #[serde(default)]
    pub options: LamportTime,
    #[serde(default)]
    pub clipboard_sharing: LamportTime,
    #[serde(default)]
    pub binaries: LamportTime,
}

impl FieldStamps {
    /// Stamp every field with the same value (used for the initial
    /// per-machine bootstrap).
    pub fn all(stamp: LamportTime) -> Self {
        FieldStamps {
            role: stamp.clone(),
            server_name: stamp.clone(),
            server_address: stamp.clone(),
            port: stamp.clone(),
            service_port: stamp.clone(),
            screens: stamp.clone(),
            links: stamp.clone(),
            options: stamp.clone(),
            clipboard_sharing: stamp.clone(),
            binaries: stamp,
        }
    }

    fn max_counter(&self) -> u64 {
        [
            self.role.counter,
            self.server_name.counter,
            self.server_address.counter,
            self.port.counter,
            self.service_port.counter,
            self.screens.counter,
            self.links.counter,
            self.options.counter,
            self.clipboard_sharing.counter,
            self.binaries.counter,
        ]
        .into_iter()
        .max()
        .unwrap_or(0)
    }
}

/// Outcome of [`VersionedConfig::merge`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeOutcome {
    /// The merge produced no change — incoming was already covered by our
    /// stamps. Callers should NOT rebroadcast in this case (prevents
    /// gossip loops).
    NoChange,
    /// At least one field was overwritten by the peer's higher stamp.
    Updated,
}

/// A [`Config`] plus the Lamport stamps that make it merge-able.
///
/// `clock` is the local Lamport counter — bumped on every local edit and
/// raised to `max(self, peer)` on every merge. Subsequent local edits
/// always carry a stamp higher than anything we've ever observed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VersionedConfig {
    pub config: Config,
    #[serde(default)]
    pub stamps: FieldStamps,
    #[serde(default)]
    pub clock: u64,
}

impl VersionedConfig {
    /// Construct an initial versioned config: every field carries the same
    /// `(counter=1, origin)` stamp so peers that have never edited any
    /// field also have an unambiguous owner for tiebreaks.
    pub fn initial(config: Config, origin: &str) -> Self {
        let stamp = LamportTime {
            counter: 1,
            origin: origin.to_string(),
        };
        VersionedConfig {
            config,
            stamps: FieldStamps::all(stamp),
            clock: 1,
        }
    }

    /// Construct from a config + sidecar-loaded stamps. Used when reading
    /// from disk; the stamps may be partially defaulted if the sidecar is
    /// older than the current schema.
    pub fn from_parts(config: Config, stamps: FieldStamps, clock: u64) -> Self {
        VersionedConfig {
            config,
            stamps,
            clock,
        }
    }

    /// SHA-256 over the canonical JSON of the *shared* subset of the
    /// config (the layout fields that get LWW-merged across peers, plus
    /// their stamps). Two peers that have converged on the shared layout
    /// produce identical hashes regardless of their local `role`,
    /// `server_name`, etc. — otherwise discovery-driven sync would loop
    /// forever, since per-machine fields differ by definition.
    ///
    /// Truncated to 16 hex chars for human-readable TXT records; callers
    /// that need full collision resistance should use the full hash
    /// directly.
    pub fn head_hash(&self) -> String {
        let shared = SharedHashInput {
            port: self.config.port,
            screens: &self.config.screens,
            links: &self.config.links,
            options: &self.config.options,
            clipboard_sharing: self.config.clipboard_sharing,
            port_stamp: &self.stamps.port,
            screens_stamp: &self.stamps.screens,
            links_stamp: &self.stamps.links,
            options_stamp: &self.stamps.options,
            clipboard_sharing_stamp: &self.stamps.clipboard_sharing,
        };
        let bytes = match serde_json::to_vec(&shared) {
            Ok(b) => b,
            Err(_) => return String::new(),
        };
        let digest = Sha256::digest(&bytes);
        hex::encode(&digest[..8])
    }

    /// Apply a *local* edit. For each field that differs from the prior
    /// config, set its stamp to a fresh `(clock+1, origin)`. Returns
    /// `true` if anything changed (so the caller can decide whether to
    /// rebroadcast).
    pub fn apply_local(&mut self, new_config: Config, origin: &str) -> bool {
        if self.config == new_config {
            return false;
        }
        // Single bump per local edit, even if it touches several fields —
        // every changed field gets the same Lamport stamp, which is fine
        // because a single user action is atomic from our point of view.
        self.clock = self.clock.max(self.stamps.max_counter()) + 1;
        let stamp = LamportTime {
            counter: self.clock,
            origin: origin.to_string(),
        };

        if self.config.role != new_config.role {
            self.stamps.role = stamp.clone();
        }
        if self.config.server_name != new_config.server_name {
            self.stamps.server_name = stamp.clone();
        }
        if self.config.server_address != new_config.server_address {
            self.stamps.server_address = stamp.clone();
        }
        if self.config.port != new_config.port {
            self.stamps.port = stamp.clone();
        }
        if self.config.service_port != new_config.service_port {
            self.stamps.service_port = stamp.clone();
        }
        if self.config.screens != new_config.screens {
            self.stamps.screens = stamp.clone();
        }
        if self.config.links != new_config.links {
            self.stamps.links = stamp.clone();
        }
        if self.config.options != new_config.options {
            self.stamps.options = stamp.clone();
        }
        if self.config.clipboard_sharing != new_config.clipboard_sharing {
            self.stamps.clipboard_sharing = stamp.clone();
        }
        if self.config.binaries != new_config.binaries {
            self.stamps.binaries = stamp;
        }

        self.config = new_config;
        true
    }

    /// Merge `other` into `self` using per-field LWW for *shared* fields
    /// only. Returns [`MergeOutcome::NoChange`] if all of `other`'s stamps
    /// for the shared fields were dominated by ours — i.e. `other` brought
    /// us nothing new.
    ///
    /// ### Shared vs per-machine fields
    ///
    /// Only the layout (`screens`, `links`, `options`) and the Synergy
    /// `port` are merged. Every other field describes the *local machine*
    /// and must never be overwritten by a peer:
    ///
    /// * `role` — what this machine does (server vs client).
    /// * `server_name` — the screen name this machine advertises (which
    ///   screen in `screens` it actually *is*).
    /// * `server_address` — only meaningful when this machine is a client.
    ///   Syncing it would point a server at itself.
    /// * `service_port` — the local synbad daemon's pairing port.
    /// * `binaries.core` — a local filesystem path to the Core executable.
    ///
    /// We still keep the stamps for these fields in [`FieldStamps`] so
    /// existing sidecars deserialize unchanged, but they're never compared
    /// against `other`'s stamps and never applied.
    pub fn merge(&mut self, other: &VersionedConfig) -> MergeOutcome {
        // Lamport rule: receiving a message advances our clock past the
        // peer's so subsequent local edits get a stamp higher than
        // anything we've ever observed. We use the peer's full stamp range
        // (including stamps for per-machine fields) so a peer that later
        // promotes itself to gossip more fields can't suddenly produce
        // stamps lower than ours.
        self.clock = self.clock.max(other.clock).max(other.stamps.max_counter());

        let mut changed = false;

        if other.stamps.port > self.stamps.port {
            self.config.port = other.config.port;
            self.stamps.port = other.stamps.port.clone();
            changed = true;
        }
        if other.stamps.screens > self.stamps.screens {
            self.config.screens = other.config.screens.clone();
            self.stamps.screens = other.stamps.screens.clone();
            changed = true;
        }
        if other.stamps.links > self.stamps.links {
            self.config.links = other.config.links.clone();
            self.stamps.links = other.stamps.links.clone();
            changed = true;
        }
        if other.stamps.options > self.stamps.options {
            self.config.options = other.config.options.clone();
            self.stamps.options = other.stamps.options.clone();
            changed = true;
        }
        if other.stamps.clipboard_sharing > self.stamps.clipboard_sharing {
            self.config.clipboard_sharing = other.config.clipboard_sharing;
            self.stamps.clipboard_sharing = other.stamps.clipboard_sharing.clone();
            changed = true;
        }

        if changed {
            MergeOutcome::Updated
        } else {
            MergeOutcome::NoChange
        }
    }

    /// Load the sidecar that holds the stamps + clock. The config itself
    /// lives in its own TOML file alongside (`paths::config_file`); this
    /// sidecar is JSON so existing TOML parsers don't have to learn about
    /// the version model.
    ///
    /// Returns `Ok(None)` if the sidecar doesn't exist — the caller is
    /// expected to bootstrap from a fresh [`Self::initial`].
    pub fn load_sidecar(path: &Path) -> Result<Option<SideCar>, VersionedConfigError> {
        match fs::read(path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(VersionedConfigError::Io {
                path: path.into(),
                source,
            }),
        }
    }

    /// Write the stamps + clock to `path` atomically (write to a tmp file,
    /// then rename). The config itself is saved separately via
    /// `Config::save`.
    pub fn save_sidecar(&self, path: &Path) -> Result<(), VersionedConfigError> {
        let body = serde_json::to_vec_pretty(&SideCar {
            stamps: self.stamps.clone(),
            clock: self.clock,
        })?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| VersionedConfigError::Io {
                path: parent.into(),
                source,
            })?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, body).map_err(|source| VersionedConfigError::Io {
            path: tmp.clone(),
            source,
        })?;
        fs::rename(&tmp, path).map_err(|source| VersionedConfigError::Io {
            path: path.into(),
            source,
        })?;
        Ok(())
    }
}

/// On-disk shape of the version sidecar: stamps + last-seen clock. The
/// config itself stays in TOML so plain-text editing keeps working.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SideCar {
    #[serde(default)]
    pub stamps: FieldStamps,
    #[serde(default)]
    pub clock: u64,
}

/// Helper struct used only for stable hash-input ordering. Field order
/// here defines the canonical serialization for `head_hash`; do not
/// reorder without bumping the protocol version. Only the shared fields
/// appear here — see [`VersionedConfig::head_hash`].
#[derive(Serialize)]
struct SharedHashInput<'a> {
    port: u16,
    screens: &'a [synbad_config::Screen],
    links: &'a [synbad_config::Link],
    options: &'a std::collections::BTreeMap<String, String>,
    clipboard_sharing: bool,
    port_stamp: &'a LamportTime,
    screens_stamp: &'a LamportTime,
    links_stamp: &'a LamportTime,
    options_stamp: &'a LamportTime,
    clipboard_sharing_stamp: &'a LamportTime,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use synbad_config::{AudioConfig, BinaryPaths, GridPosition, Link, NodeRole, Screen, Side};

    fn config_a() -> Config {
        Config {
            role: NodeRole::Server,
            server_name: "alpha".into(),
            server_address: None,
            port: 24800,
            service_port: 24850,
            sync_port: 24851,
            screens: vec![Screen {
                name: "alpha".into(),
                aliases: vec![],
                position: GridPosition::default(),
                monitors: vec![],
            }],
            links: vec![],
            options: BTreeMap::new(),
            clipboard_sharing: true,
            autostart: true,
            binaries: BinaryPaths::default(),
            audio: AudioConfig::default(),
        }
    }

    fn config_b() -> Config {
        let mut c = config_a();
        c.screens.push(Screen {
            name: "beta".into(),
            aliases: vec![],
            position: GridPosition::default(),
            monitors: vec![],
        });
        c.links.push(Link {
            from: "alpha".into(),
            side: Side::Right,
            to: "beta".into(),
        });
        c
    }

    #[test]
    fn lamport_order_breaks_ties_by_origin() {
        let a = LamportTime {
            counter: 5,
            origin: "aaa".into(),
        };
        let b = LamportTime {
            counter: 5,
            origin: "bbb".into(),
        };
        assert!(a < b);
        assert!(b > a);
    }

    #[test]
    fn apply_local_only_bumps_changed_fields() {
        let mut v = VersionedConfig::initial(config_a(), "alpha-id");
        let role_before = v.stamps.role.clone();
        let screens_before = v.stamps.screens.clone();

        // Touch only `screens`.
        let new = config_b();
        assert!(v.apply_local(new, "alpha-id"));

        assert_eq!(
            v.stamps.role, role_before,
            "untouched fields keep their stamps"
        );
        assert!(v.stamps.screens > screens_before, "changed field bumps");
        assert!(
            v.stamps.links > screens_before,
            "links also changed → bumped"
        );
    }

    #[test]
    fn merge_lww_keeps_higher_stamp() {
        // Two peers start from the same initial state, then diverge.
        let mut alpha = VersionedConfig::initial(config_a(), "alpha-id");
        let mut beta = alpha.clone();
        // Beta's bootstrap stamp uses its own machine_id.
        beta.stamps = FieldStamps::all(LamportTime {
            counter: 1,
            origin: "beta-id".into(),
        });

        // Alpha edits a layout option; Beta independently adds a screen.
        // Both edits touch shared fields, so both should propagate.
        let mut alpha_edit = alpha.config.clone();
        alpha_edit.options.insert("heartbeat".into(), "5000".into());
        assert!(alpha.apply_local(alpha_edit, "alpha-id"));
        assert!(beta.apply_local(config_b(), "beta-id"));

        // Both peers swap state and merge. Convergence: both end with both
        // edits applied, because the edits touched different fields.
        let alpha_pre = alpha.clone();
        assert_eq!(alpha.merge(&beta), MergeOutcome::Updated);
        assert_eq!(beta.merge(&alpha_pre), MergeOutcome::Updated);

        assert_eq!(
            alpha.config.options.get("heartbeat").map(String::as_str),
            Some("5000")
        );
        assert_eq!(alpha.config.screens.len(), 2);
        assert_eq!(
            beta.config.options.get("heartbeat").map(String::as_str),
            Some("5000")
        );
        assert_eq!(beta.config.screens.len(), 2);
        assert_eq!(alpha.head_hash(), beta.head_hash(), "converged hashes");
    }

    /// `role`, `server_name`, `server_address`, `service_port`, and
    /// `binaries.core` describe the local machine and must never be
    /// overwritten by a peer. A LAN sync against an opinionated peer used
    /// to flip a client into a server (and vice versa) — this guards
    /// against that regression.
    #[test]
    fn merge_does_not_overwrite_per_machine_fields() {
        // Local machine is a client pointing at an explicit server.
        let mut local = VersionedConfig::initial(
            Config {
                role: NodeRole::Client,
                server_name: "local-host".into(),
                server_address: Some("homeserver.local".into()),
                service_port: 24850,
                binaries: BinaryPaths {
                    core: Some(std::path::PathBuf::from("/opt/local/deskflow-core")),
                },
                ..config_a()
            },
            "local-id",
        );

        // Peer pushes a contradictory state: it's a server, it has its own
        // name, a different service port, and an absolute binary path that
        // only exists on its own machine.
        let mut peer = VersionedConfig::initial(
            Config {
                role: NodeRole::Server,
                server_name: "peer-host".into(),
                server_address: None,
                service_port: 24890,
                binaries: BinaryPaths {
                    core: Some(std::path::PathBuf::from("/Users/peer/deskflow")),
                },
                ..config_b()
            },
            "peer-id",
        );
        // Peer also bumps every field on its side so its stamps dominate
        // any naive comparison.
        peer.apply_local(
            Config {
                options: {
                    let mut o = BTreeMap::new();
                    o.insert("relativeMouseMoves".into(), "true".into());
                    o
                },
                ..peer.config.clone()
            },
            "peer-id",
        );

        local.merge(&peer);

        // Per-machine fields stayed put.
        assert_eq!(local.config.role, NodeRole::Client);
        assert_eq!(local.config.server_name, "local-host");
        assert_eq!(
            local.config.server_address.as_deref(),
            Some("homeserver.local")
        );
        assert_eq!(local.config.service_port, 24850);
        assert_eq!(
            local.config.binaries.core.as_deref(),
            Some(std::path::Path::new("/opt/local/deskflow-core"))
        );
        // Shared layout *did* merge across.
        assert_eq!(
            local
                .config
                .options
                .get("relativeMouseMoves")
                .map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn clipboard_sharing_is_a_shared_field_and_merges() {
        // One peer flips off clipboard sharing; the other peer should
        // pick that up on merge so both ends agree the Core won't relay
        // the clipboard.
        let mut alpha = VersionedConfig::initial(config_a(), "alpha-id");
        let mut beta = alpha.clone();
        beta.stamps = FieldStamps::all(LamportTime {
            counter: 1,
            origin: "beta-id".into(),
        });

        let mut alpha_edit = alpha.config.clone();
        alpha_edit.clipboard_sharing = false;
        assert!(alpha.apply_local(alpha_edit, "alpha-id"));
        assert!(!alpha.config.clipboard_sharing);

        let alpha_pre = alpha.clone();
        assert_eq!(beta.merge(&alpha), MergeOutcome::Updated);
        beta.merge(&alpha_pre);
        assert!(!beta.config.clipboard_sharing);

        // A second merge from the same alpha snapshot is now a no-op —
        // both sides agree on clipboard_sharing's stamp.
        assert_eq!(beta.merge(&alpha_pre), MergeOutcome::NoChange);
    }

    #[test]
    fn merge_is_noop_when_we_already_dominate() {
        let v = VersionedConfig::initial(config_a(), "alpha-id");
        let stale = VersionedConfig::initial(config_a(), "alpha-id");
        let mut copy = v.clone();
        assert_eq!(copy.merge(&stale), MergeOutcome::NoChange);
        assert_eq!(copy.head_hash(), v.head_hash());
    }

    #[test]
    fn concurrent_edit_to_same_field_breaks_tie_by_origin() {
        let mut alpha = VersionedConfig::initial(config_a(), "alpha-id");
        let mut beta = VersionedConfig::initial(config_a(), "beta-id");

        // Both edit `port` (a shared field) from the same start state —
        // counters will collide, origin breaks the tie. With
        // `alpha-id < beta-id`, beta wins.
        let mut a_edit = alpha.config.clone();
        a_edit.port = 33001;
        let mut b_edit = beta.config.clone();
        b_edit.port = 33002;
        alpha.apply_local(a_edit, "alpha-id");
        beta.apply_local(b_edit, "beta-id");
        assert_eq!(alpha.stamps.port.counter, beta.stamps.port.counter);

        let alpha_pre = alpha.clone();
        alpha.merge(&beta);
        beta.merge(&alpha_pre);

        assert_eq!(alpha.config.port, 33002);
        assert_eq!(beta.config.port, 33002);
    }

    #[test]
    fn head_hash_is_deterministic() {
        let v1 = VersionedConfig::initial(config_a(), "alpha-id");
        let v2 = VersionedConfig::initial(config_a(), "alpha-id");
        assert_eq!(v1.head_hash(), v2.head_hash());
        // Changing the config changes the hash.
        let mut v3 = v1.clone();
        v3.apply_local(config_b(), "alpha-id");
        assert_ne!(v1.head_hash(), v3.head_hash());
    }

    #[test]
    fn sidecar_roundtrips() {
        let tmp = std::env::temp_dir().join(format!(
            "synbad-sync-sidecar-test-{}-{}.json",
            std::process::id(),
            std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos()
        ));
        let mut v = VersionedConfig::initial(config_a(), "alpha-id");
        v.apply_local(config_b(), "alpha-id");
        v.save_sidecar(&tmp).unwrap();
        let loaded = VersionedConfig::load_sidecar(&tmp).unwrap().unwrap();
        assert_eq!(loaded.stamps, v.stamps);
        assert_eq!(loaded.clock, v.clock);
        let _ = std::fs::remove_file(&tmp);
    }
}
