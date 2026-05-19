//! Trusted-peer store.
//!
//! After a successful pairing (see `synbadd::pairing`), the responder's
//! ed25519 public key is persisted here. Only peers in this store are
//! allowed to participate in config sync or be added to a screen layout.
//!
//! The store is a plain JSON file at
//! `~/.config/synbad/trusted-peers.json`. We don't bother encrypting
//! it — these are public keys, not secrets. If the file is removed,
//! pairing is lost and must be redone.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

// `TrustedPeer` lives in `synbad-ipc` so the GUI gets it without
// transitively compiling the crypto stack.
pub use synbad_ipc::TrustedPeer;

#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    #[error("io at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("malformed trusted-peers JSON: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustFile {
    #[serde(default)]
    peers: Vec<TrustedPeer>,
}

pub struct TrustedPeerStore {
    path: PathBuf,
    peers: Vec<TrustedPeer>,
}

impl TrustedPeerStore {
    /// Load from `path`; missing file → empty store.
    pub fn load(path: &Path) -> Result<Self, TrustError> {
        let peers = match fs::read(path) {
            Ok(bytes) => {
                let f: TrustFile = serde_json::from_slice(&bytes)?;
                f.peers
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(source) => {
                return Err(TrustError::Io {
                    path: path.into(),
                    source,
                })
            }
        };
        Ok(TrustedPeerStore {
            path: path.to_path_buf(),
            peers,
        })
    }

    pub fn list(&self) -> &[TrustedPeer] {
        &self.peers
    }

    pub fn contains(&self, machine_id: &str) -> bool {
        self.peers.iter().any(|p| p.machine_id == machine_id)
    }

    pub fn get(&self, machine_id: &str) -> Option<&TrustedPeer> {
        self.peers.iter().find(|p| p.machine_id == machine_id)
    }

    /// Insert or update (matching on `machine_id`) and persist to disk.
    pub fn upsert(&mut self, peer: TrustedPeer) -> Result<(), TrustError> {
        if let Some(slot) = self
            .peers
            .iter_mut()
            .find(|p| p.machine_id == peer.machine_id)
        {
            *slot = peer;
        } else {
            self.peers.push(peer);
        }
        self.save()
    }

    /// Remove a peer (e.g. user revokes trust). Returns true if removed.
    pub fn remove(&mut self, machine_id: &str) -> Result<bool, TrustError> {
        let before = self.peers.len();
        self.peers.retain(|p| p.machine_id != machine_id);
        if self.peers.len() != before {
            self.save()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn save(&self) -> Result<(), TrustError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|source| TrustError::Io {
                path: parent.into(),
                source,
            })?;
        }
        let body = serde_json::to_vec_pretty(&TrustFile {
            peers: self.peers.clone(),
        })?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, body).map_err(|source| TrustError::Io {
            path: tmp.clone(),
            source,
        })?;
        fs::rename(&tmp, &self.path).map_err(|source| TrustError::Io {
            path: self.path.clone(),
            source,
        })?;
        Ok(())
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: &str) -> TrustedPeer {
        TrustedPeer {
            machine_id: id.into(),
            display_name: format!("peer-{}", id),
            public_key_hex: "00".repeat(32),
            fingerprint: "aaaa-bbbb-cccc-dddd".into(),
            paired_at_unix: 1_700_000_000,
        }
    }

    fn tempfile(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "synbad-trust-{}-{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4(),
            name
        ));
        let _ = fs::remove_file(&p);
        p
    }

    #[test]
    fn upsert_and_reload() {
        let path = tempfile("upsert");
        let mut store = TrustedPeerStore::load(&path).unwrap();
        store.upsert(sample("aaa")).unwrap();
        store.upsert(sample("bbb")).unwrap();
        // Update existing.
        let mut p = sample("aaa");
        p.display_name = "renamed".into();
        store.upsert(p).unwrap();

        let reloaded = TrustedPeerStore::load(&path).unwrap();
        assert_eq!(reloaded.peers.len(), 2);
        assert_eq!(reloaded.get("aaa").unwrap().display_name, "renamed");
        assert!(reloaded.contains("bbb"));
    }

    #[test]
    fn remove_reports_outcome() {
        let path = tempfile("remove");
        let mut store = TrustedPeerStore::load(&path).unwrap();
        store.upsert(sample("xx")).unwrap();
        assert!(store.remove("xx").unwrap());
        assert!(!store.remove("xx").unwrap());
    }
}
