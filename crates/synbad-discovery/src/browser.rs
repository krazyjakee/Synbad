//! mDNS service browser.
//!
//! Watches for `_synbad._tcp.local.` instances on the LAN. Resolved peers
//! are emitted as [`DiscoveryEvent`] values into a tokio channel so the
//! daemon's main `select!` loop can consume them like any other input.
//!
//! Filters out our own advertisement by machine_id so we don't get a
//! self-loop event on every start.

use std::thread;

use mdns_sd::{ServiceDaemon, ServiceEvent};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::SERVICE_TYPE;
use synbad_ipc::DiscoveredPeer;

#[derive(Debug, thiserror::Error)]
pub enum BrowseError {
    #[error("mdns: {0}")]
    Mdns(#[from] mdns_sd::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DiscoveryEvent {
    Found(DiscoveredPeer),
    Lost { machine_id: String },
}

pub struct Browser {
    daemon: ServiceDaemon,
}

impl Browser {
    /// Start browsing. `own_machine_id` filters self-discovery. Returns
    /// a receiver of [`DiscoveryEvent`]s.
    pub fn start(
        own_machine_id: &str,
    ) -> Result<(Self, mpsc::Receiver<DiscoveryEvent>), BrowseError> {
        let daemon = ServiceDaemon::new()?;
        let raw_rx = daemon.browse(SERVICE_TYPE)?;
        let (tx, rx) = mpsc::channel::<DiscoveryEvent>(64);

        let own_id = own_machine_id.to_string();
        thread::Builder::new()
            .name("synbad-discovery-browser".into())
            .spawn(move || pump_events(raw_rx, tx, own_id))
            .expect("spawn discovery browser thread");

        Ok((Browser { daemon }, rx))
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

fn pump_events(
    raw_rx: mdns_sd::Receiver<ServiceEvent>,
    tx: mpsc::Sender<DiscoveryEvent>,
    own_id: String,
) {
    // mdns-sd's `ServiceRemoved` carries the full DNS name, not the TXT
    // payload — so we have to remember which machine_id each full_name
    // resolved to, in order to emit a `Lost { machine_id }` the supervisor
    // can match.
    let mut full_to_id: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    while let Ok(ev) = raw_rx.recv() {
        let to_send = match ev {
            ServiceEvent::ServiceResolved(info) => {
                let Some(peer) = peer_from(&info) else {
                    continue;
                };
                if peer.machine_id == own_id {
                    continue;
                }
                full_to_id.insert(info.get_fullname().to_string(), peer.machine_id.clone());
                Some(DiscoveryEvent::Found(peer))
            }
            ServiceEvent::ServiceRemoved(_kind, full_name) => full_to_id
                .remove(&full_name)
                .map(|machine_id| DiscoveryEvent::Lost { machine_id }),
            ServiceEvent::SearchStarted(_)
            | ServiceEvent::SearchStopped(_)
            | ServiceEvent::ServiceFound(_, _) => None,
        };

        if let Some(ev) = to_send {
            // Block the browser thread on backpressure rather than dropping
            // events — discovery throughput is low.
            if tx.blocking_send(ev).is_err() {
                // Receiver dropped; daemon is shutting down.
                return;
            }
        }
    }
}

fn peer_from(info: &mdns_sd::ServiceInfo) -> Option<DiscoveredPeer> {
    let txt = info.get_properties();
    let machine_id = txt.get_property_val_str("id")?.to_string();
    let fingerprint = txt.get_property_val_str("fp")?.to_string();
    let protocol_version = txt
        .get_property_val_str("v")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);

    let display_name = info
        .get_fullname()
        .split('.')
        .next()
        .unwrap_or("")
        .to_string();

    // Prefer the first resolved IPv4 address as the host. Fall back to the
    // hostname mDNS associates with the record so cross-platform name
    // resolution still works.
    let host = info
        .get_addresses()
        .iter()
        .find_map(|addr| match addr {
            std::net::IpAddr::V4(v4) => Some(v4.to_string()),
            _ => None,
        })
        .or_else(|| info.get_addresses().iter().next().map(|a| a.to_string()))
        .unwrap_or_else(|| info.get_hostname().to_string());

    // SRV port = the Synbad daemon's pairing port (what we connect to for
    // the pairing handshake).
    // TXT `sync_port` = the Synbad daemon's config-sync port (separate
    // lifecycle from pairing). Zero means the peer didn't advertise one;
    // sync to that peer isn't possible.
    // TXT `core_port` = the Synergy/Deskflow Core's input port (informational,
    // used by the GUI to wire up layout entries).
    // TXT `cfg` = short hash of the peer's VersionedConfig head; empty
    // string means the peer didn't advertise one (treated as "unknown" —
    // any local edit will still push to that peer).
    let core_port = txt
        .get_property_val_str("core_port")
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let sync_port = txt
        .get_property_val_str("sync_port")
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let config_head = txt.get_property_val_str("cfg").unwrap_or("").to_string();

    Some(DiscoveredPeer {
        machine_id,
        display_name,
        host,
        service_port: info.get_port(),
        core_port,
        sync_port,
        fingerprint,
        protocol_version,
        config_head,
    })
}
