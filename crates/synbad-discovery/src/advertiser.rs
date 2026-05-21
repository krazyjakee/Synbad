//! mDNS service advertiser.
//!
//! Publishes a `_synbad._tcp.local.` service instance with our identity
//! TXT keys (`v`, `id`, `host`, `port`, `fp`). Drops automatically
//! deregister and broadcast a goodbye packet — other peers reap us
//! promptly instead of waiting for the TTL.

use std::collections::HashMap;

use mdns_sd::{ServiceDaemon, ServiceInfo};

use crate::{Identity, PROTOCOL_VERSION, SERVICE_TYPE};

#[derive(Debug, thiserror::Error)]
pub enum AdvertiseError {
    #[error("mdns: {0}")]
    Mdns(#[from] mdns_sd::Error),
    #[error("invalid display name: {0:?}")]
    BadName(String),
}

/// Inputs that go into the published TXT record. Stored on the
/// [`Advertiser`] so live mutations (currently just `audio_port`) can
/// re-build a `ServiceInfo` with the same identity but a fresh TXT.
struct AdvertisedFields {
    machine_id: String,
    fingerprint: String,
    display_name: String,
    host_name: String,
    service_port: u16,
    sync_port: u16,
    core_port: u16,
    audio_port: u16,
    config_head: String,
}

pub struct Advertiser {
    daemon: ServiceDaemon,
    full_name: String,
    fields: AdvertisedFields,
}

impl Advertiser {
    /// Start advertising. `display_name` is the user-facing label for the
    /// service instance (e.g. machine hostname). `service_port` is the TCP
    /// port the Synbad daemon listens on for pairing — peers connect to
    /// this port to pair with us. `sync_port` is the TCP port the daemon
    /// listens on for config-sync sessions (separate from pairing so each
    /// has an independent lifecycle and so a sync flood can't starve
    /// pairing). `core_port` is the Synergy/Deskflow Core's input-sharing
    /// port (advertised as TXT for peers that want to wire up a layout
    /// entry directly). `config_head` is the short hash of our current
    /// `VersionedConfig` — peers compare it against theirs to detect
    /// divergence; pass an empty string if config sync isn't active yet.
    pub fn start(
        identity: &Identity,
        display_name: &str,
        service_port: u16,
        sync_port: u16,
        core_port: u16,
        audio_port: u16,
        config_head: &str,
    ) -> Result<Self, AdvertiseError> {
        if display_name.is_empty() || display_name.contains('.') {
            return Err(AdvertiseError::BadName(display_name.to_string()));
        }

        let daemon = ServiceDaemon::new()?;

        // Use the machine hostname for the mDNS host record so peers can
        // resolve it via the same daemon's A records. Append `.local.` if
        // it's missing — mdns-sd requires the trailing dot.
        let host_name = match hostname() {
            Ok(h) if !h.is_empty() => ensure_local_dot(&h),
            _ => format!("{}.local.", sanitize(display_name)),
        };

        let fields = AdvertisedFields {
            machine_id: identity.machine_id.to_string(),
            fingerprint: identity.fingerprint.clone(),
            display_name: display_name.to_string(),
            host_name,
            service_port,
            sync_port,
            core_port,
            audio_port,
            config_head: config_head.to_string(),
        };

        let service = build_service_info(&fields)?;
        let full_name = service.get_fullname().to_string();
        daemon.register(service)?;
        tracing::info!(%full_name, "mDNS service registered");

        Ok(Advertiser {
            daemon,
            full_name,
            fields,
        })
    }

    /// Re-publish the TXT record with a new `audio_port` (use `0` to drop
    /// the `audio_port` key entirely). Cheap no-op when the value hasn't
    /// changed.
    ///
    /// mdns-sd 0.11 has no in-place TXT update, so we unregister the
    /// existing record and register a fresh one against the same daemon.
    /// Peers' browsers see this as a `ServiceRemoved` followed by a fresh
    /// `ServiceResolved` carrying the new TXT — which is exactly what the
    /// receiving supervisor needs to refresh its cached `DiscoveredPeer`
    /// entry and dial us.
    pub fn set_audio_port(&mut self, audio_port: u16) -> Result<(), AdvertiseError> {
        if self.fields.audio_port == audio_port {
            return Ok(());
        }
        self.fields.audio_port = audio_port;
        // Best-effort unregister — if mdns-sd has already evicted us
        // (interface flap, etc.) we still want the re-register to land.
        let _ = self.daemon.unregister(&self.full_name);
        let service = build_service_info(&self.fields)?;
        let new_full_name = service.get_fullname().to_string();
        self.daemon.register(service)?;
        self.full_name = new_full_name;
        tracing::info!(
            full_name = %self.full_name,
            audio_port,
            "mDNS service re-registered (audio_port updated)"
        );
        Ok(())
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        // Best-effort: unregister so peers see the goodbye immediately and
        // don't wait for TTL expiry.
        let _ = self.daemon.unregister(&self.full_name);
        let _ = self.daemon.shutdown();
    }
}

fn build_service_info(f: &AdvertisedFields) -> Result<ServiceInfo, AdvertiseError> {
    let mut props: HashMap<String, String> = HashMap::new();
    props.insert("v".into(), PROTOCOL_VERSION.to_string());
    props.insert("id".into(), f.machine_id.clone());
    props.insert("fp".into(), f.fingerprint.clone());
    props.insert("core_port".into(), f.core_port.to_string());
    props.insert("sync_port".into(), f.sync_port.to_string());
    // Only advertise the audio port when audio is actually running —
    // a zero would be misleading and the lookup falls back to "no
    // outbound dial" on the peer's side anyway.
    if f.audio_port != 0 {
        props.insert("audio_port".into(), f.audio_port.to_string());
    }
    if !f.config_head.is_empty() {
        props.insert("cfg".into(), f.config_head.clone());
    }
    // `host` is informational — peers should use mDNS-resolved A/AAAA
    // records to connect. Useful for diagnostics when several names
    // share a host.
    if let Ok(host) = hostname() {
        props.insert("host".into(), host);
    }

    Ok(ServiceInfo::new(
        SERVICE_TYPE,
        &f.display_name,
        &f.host_name,
        "",
        f.service_port,
        props,
    )?
    // Let mdns-sd auto-pick addresses on the local interfaces, so we
    // don't have to enumerate them ourselves. Required when we pass
    // an empty `ip` string above.
    .enable_addr_auto())
}

fn hostname() -> std::io::Result<String> {
    // Avoid pulling in the `gethostname` crate just for this — read
    // /proc on Linux, fall back to env vars elsewhere. Empty string on
    // failure is fine; the advertiser substitutes the display name.
    #[cfg(unix)]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
            return Ok(s.trim().to_string());
        }
        if let Ok(s) = std::env::var("HOSTNAME") {
            return Ok(s);
        }
    }
    #[cfg(windows)]
    {
        if let Ok(s) = std::env::var("COMPUTERNAME") {
            return Ok(s);
        }
    }
    Ok(String::new())
}

fn ensure_local_dot(host: &str) -> String {
    let h = host.trim_end_matches('.');
    if h.ends_with(".local") {
        format!("{}.", h)
    } else {
        format!("{}.local.", sanitize(h))
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}
