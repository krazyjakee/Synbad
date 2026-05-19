//! Persistent per-machine identity.
//!
//! Synbad keeps three small files in the user's config dir so a host has a
//! stable identity across IP / hostname changes and reboots:
//!
//! - `identity/machine-id`     — UUID v4 (text), the `id` TXT key.
//! - `identity/ed25519.secret` — 32 raw bytes, mode 0600 on Unix.
//! - `identity/ed25519.public` — 32 raw bytes; rederivable from the secret
//!   but cached for fast startup.
//!
//! The **fingerprint** the user sees during pairing is
//! `SHA-256(public_key)` truncated to the first 8 bytes and rendered as
//! `aaaa-bbbb-cccc-dddd`. 64 bits is short enough to read aloud and long
//! enough that an attacker can't fluke a collision in a pairing session.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ed25519_dalek::{SigningKey, VerifyingKey, PUBLIC_KEY_LENGTH, SECRET_KEY_LENGTH};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid machine-id at {path}")]
    BadMachineId { path: PathBuf },
    #[error("ed25519 key file at {path} is the wrong size")]
    BadKeyLength { path: PathBuf },
}

#[derive(Clone)]
pub struct Identity {
    pub machine_id: Uuid,
    pub public_key: [u8; PUBLIC_KEY_LENGTH],
    secret_key: [u8; SECRET_KEY_LENGTH],
    pub fingerprint: String,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never let the secret key into Debug output.
        f.debug_struct("Identity")
            .field("machine_id", &self.machine_id)
            .field("fingerprint", &self.fingerprint)
            .field("public_key", &hex::encode(self.public_key))
            .finish_non_exhaustive()
    }
}

impl Identity {
    /// Load identity from `dir`; if any file is missing, regenerate it.
    /// The directory is created if needed.
    pub fn load_or_create(dir: &Path) -> Result<Self, IdentityError> {
        fs::create_dir_all(dir).map_err(|source| IdentityError::Io {
            path: dir.to_path_buf(),
            source,
        })?;

        let machine_id = load_or_create_machine_id(dir)?;
        let (secret_key, public_key) = load_or_create_keypair(dir)?;
        let fingerprint = fingerprint_for(&public_key);

        Ok(Identity {
            machine_id,
            public_key,
            secret_key,
            fingerprint,
        })
    }

    /// Borrow the secret key for signing. Kept private-by-convention; only
    /// the pairing protocol should reach in for this, never the GUI.
    pub fn signing_key(&self) -> SigningKey {
        SigningKey::from_bytes(&self.secret_key)
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key().verifying_key()
    }
}

fn load_or_create_machine_id(dir: &Path) -> Result<Uuid, IdentityError> {
    let path = dir.join("machine-id");
    match fs::read_to_string(&path) {
        Ok(s) => Uuid::parse_str(s.trim())
            .map_err(|_| IdentityError::BadMachineId { path: path.clone() }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let id = Uuid::new_v4();
            fs::write(&path, id.to_string()).map_err(|source| IdentityError::Io {
                path: path.clone(),
                source,
            })?;
            Ok(id)
        }
        Err(source) => Err(IdentityError::Io { path, source }),
    }
}

fn load_or_create_keypair(
    dir: &Path,
) -> Result<([u8; SECRET_KEY_LENGTH], [u8; PUBLIC_KEY_LENGTH]), IdentityError> {
    let secret_path = dir.join("ed25519.secret");
    let public_path = dir.join("ed25519.public");

    if secret_path.exists() {
        let secret = fs::read(&secret_path).map_err(|source| IdentityError::Io {
            path: secret_path.clone(),
            source,
        })?;
        let secret: [u8; SECRET_KEY_LENGTH] = secret
            .try_into()
            .map_err(|_| IdentityError::BadKeyLength { path: secret_path.clone() })?;
        let signing = SigningKey::from_bytes(&secret);
        let public = signing.verifying_key().to_bytes();
        // Re-cache the public key in case it was deleted.
        let _ = fs::write(&public_path, public);
        return Ok((secret, public));
    }

    let mut rng = rand_core::OsRng;
    let signing = SigningKey::generate(&mut rng);
    let secret = signing.to_bytes();
    let public = signing.verifying_key().to_bytes();

    fs::write(&secret_path, secret).map_err(|source| IdentityError::Io {
        path: secret_path.clone(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(&secret_path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = fs::set_permissions(&secret_path, perms);
        }
    }
    fs::write(&public_path, public).map_err(|source| IdentityError::Io {
        path: public_path,
        source,
    })?;

    Ok((secret, public))
}

/// User-facing fingerprint string. Derived deterministically from the
/// public key so two peers can compare what they each see.
pub fn fingerprint_for(public_key: &[u8; PUBLIC_KEY_LENGTH]) -> String {
    let digest = Sha256::digest(public_key);
    // 8 bytes = 16 hex chars = ~64 bits. Plenty for a pairing-session check.
    let hex = hex::encode(&digest[..8]);
    // Insert hyphens every 4 chars: aaaa-bbbb-cccc-dddd
    let mut out = String::with_capacity(19);
    for (i, c) in hex.chars().enumerate() {
        if i > 0 && i % 4 == 0 {
            out.push('-');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_deterministic_and_grouped() {
        let pk = [0u8; PUBLIC_KEY_LENGTH];
        let fp = fingerprint_for(&pk);
        assert_eq!(fp.len(), 19);
        assert!(fp.chars().filter(|c| *c == '-').count() == 3);
        // Same key → same fingerprint.
        assert_eq!(fp, fingerprint_for(&pk));
    }

    #[test]
    fn load_or_create_persists_across_calls() {
        let tmp = tempdir();
        let a = Identity::load_or_create(&tmp).unwrap();
        let b = Identity::load_or_create(&tmp).unwrap();
        assert_eq!(a.machine_id, b.machine_id);
        assert_eq!(a.public_key, b.public_key);
        assert_eq!(a.fingerprint, b.fingerprint);
    }

    fn tempdir() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "synbad-discovery-tests-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
