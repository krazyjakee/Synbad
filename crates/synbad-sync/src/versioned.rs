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
        let stamp = LamportTime { counter: 1, origin: origin.to_string() };
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
        VersionedConfig { config, stamps, clock }
    }

    /// SHA-256 over the canonical JSON of `(config, stamps)`. Two peers
    /// running the same schema produce identical hashes for identical
    /// state. Truncated to 16 hex chars for human-readable TXT records;
    /// callers that need full collision resistance should use the full
    /// hash directly.
    pub fn head_hash(&self) -> String {
        let bytes = match serde_json::to_vec(&HashInput {
            config: &self.config,
            stamps: &self.stamps,
        }) {
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
        let stamp = LamportTime { counter: self.clock, origin: origin.to_string() };

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
        if self.config.binaries != new_config.binaries {
            self.stamps.binaries = stamp;
        }

        self.config = new_config;
        true
    }

    /// Merge `other` into `self` using per-field LWW. Returns
    /// [`MergeOutcome::NoChange`] if all of `other`'s stamps were
    /// dominated by ours — i.e. `other` brought us nothing new.
    pub fn merge(&mut self, other: &VersionedConfig) -> MergeOutcome {
        // Lamport rule: receiving a message advances our clock past the
        // peer's so subsequent local edits get a stamp higher than
        // anything we've ever observed.
        self.clock = self.clock.max(other.clock).max(other.stamps.max_counter());

        let mut changed = false;

        if other.stamps.role > self.stamps.role {
            self.config.role = other.config.role;
            self.stamps.role = other.stamps.role.clone();
            changed = true;
        }
        if other.stamps.server_name > self.stamps.server_name {
            self.config.server_name = other.config.server_name.clone();
            self.stamps.server_name = other.stamps.server_name.clone();
            changed = true;
        }
        if other.stamps.server_address > self.stamps.server_address {
            self.config.server_address = other.config.server_address.clone();
            self.stamps.server_address = other.stamps.server_address.clone();
            changed = true;
        }
        if other.stamps.port > self.stamps.port {
            self.config.port = other.config.port;
            self.stamps.port = other.stamps.port.clone();
            changed = true;
        }
        if other.stamps.service_port > self.stamps.service_port {
            self.config.service_port = other.config.service_port;
            self.stamps.service_port = other.stamps.service_port.clone();
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
        if other.stamps.binaries > self.stamps.binaries {
            self.config.binaries = other.config.binaries.clone();
            self.stamps.binaries = other.stamps.binaries.clone();
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
            Err(source) => Err(VersionedConfigError::Io { path: path.into(), source }),
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
/// reorder without bumping the protocol version.
#[derive(Serialize)]
struct HashInput<'a> {
    config: &'a Config,
    stamps: &'a FieldStamps,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use synbad_config::{BinaryPaths, GridPosition, Link, NodeRole, Screen, Side};

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
            }],
            links: vec![],
            options: BTreeMap::new(),
            binaries: BinaryPaths::default(),
        }
    }

    fn config_b() -> Config {
        let mut c = config_a();
        c.screens.push(Screen {
            name: "beta".into(),
            aliases: vec![],
            position: GridPosition::default(),
        });
        c.links.push(Link { from: "alpha".into(), side: Side::Right, to: "beta".into() });
        c
    }

    #[test]
    fn lamport_order_breaks_ties_by_origin() {
        let a = LamportTime { counter: 5, origin: "aaa".into() };
        let b = LamportTime { counter: 5, origin: "bbb".into() };
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

        assert_eq!(v.stamps.role, role_before, "untouched fields keep their stamps");
        assert!(v.stamps.screens > screens_before, "changed field bumps");
        assert!(v.stamps.links > screens_before, "links also changed → bumped");
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

        // Alpha edits server_name; Beta independently adds a screen.
        let mut alpha_edit = alpha.config.clone();
        alpha_edit.server_name = "alpha-renamed".into();
        // server_name must also match a screen for validate() to pass,
        // but we're testing merge directly so skip validation.
        assert!(alpha.apply_local(alpha_edit, "alpha-id"));
        assert!(beta.apply_local(config_b(), "beta-id"));

        // Both peers swap state and merge. Convergence: both end with both
        // edits applied, because the edits touched different fields.
        let alpha_pre = alpha.clone();
        assert_eq!(alpha.merge(&beta), MergeOutcome::Updated);
        assert_eq!(beta.merge(&alpha_pre), MergeOutcome::Updated);

        assert_eq!(alpha.config.server_name, "alpha-renamed");
        assert_eq!(alpha.config.screens.len(), 2);
        assert_eq!(beta.config.server_name, "alpha-renamed");
        assert_eq!(beta.config.screens.len(), 2);
        assert_eq!(alpha.head_hash(), beta.head_hash(), "converged hashes");
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

        // Both edit `server_name` from the same start state — counters will
        // collide, origin breaks the tie. With `alpha-id < beta-id`, beta
        // wins.
        let mut a_edit = alpha.config.clone();
        a_edit.server_name = "from-alpha".into();
        let mut b_edit = beta.config.clone();
        b_edit.server_name = "from-beta".into();
        alpha.apply_local(a_edit, "alpha-id");
        beta.apply_local(b_edit, "beta-id");
        assert_eq!(alpha.stamps.server_name.counter, beta.stamps.server_name.counter);

        let alpha_pre = alpha.clone();
        alpha.merge(&beta);
        beta.merge(&alpha_pre);

        assert_eq!(alpha.config.server_name, "from-beta");
        assert_eq!(beta.config.server_name, "from-beta");
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
