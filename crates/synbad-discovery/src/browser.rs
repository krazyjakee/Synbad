//! mDNS service browser.
//!
//! Watches for `_synbad._tcp.local.` instances on the LAN. Resolved peers
//! are emitted as [`DiscoveryEvent`] values into a tokio channel so the
//! daemon's main `select!` loop can consume them like any other input.
//!
//! Filters out our own advertisement by machine_id so we don't get a
//! self-loop event on every start.

use std::thread;
use std::time::{Duration, Instant};

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
        let mut interfaces = crate::interfaces::InterfacePolicy::default();
        interfaces.refresh(&daemon)?;
        let raw_rx = daemon.browse(SERVICE_TYPE)?;
        let monitor = daemon.monitor()?;
        let (tx, rx) = mpsc::channel::<DiscoveryEvent>(64);

        let own_id = own_machine_id.to_string();
        let worker_daemon = daemon.clone();
        thread::Builder::new()
            .name("synbad-discovery-browser".into())
            .spawn(move || pump_events(worker_daemon, raw_rx, monitor, interfaces, tx, own_id))
            .expect("spawn discovery browser thread");

        Ok((Browser { daemon }, rx))
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        if let Ok(rx) = self.daemon.shutdown() {
            let _ = rx.recv_timeout(Duration::from_secs(1));
        }
    }
}

fn pump_events(
    daemon: ServiceDaemon,
    mut raw_rx: mdns_sd::Receiver<ServiceEvent>,
    monitor: mdns_sd::Receiver<mdns_sd::DaemonEvent>,
    mut interfaces: crate::interfaces::InterfacePolicy,
    tx: mpsc::Sender<DiscoveryEvent>,
    own_id: String,
) {
    // mdns-sd's `ServiceRemoved` carries the full DNS name, not the TXT
    // payload — so we have to remember which machine_id each full_name
    // resolved to, in order to emit a `Lost { machine_id }` the supervisor
    // can match.
    let mut full_to_peer: std::collections::HashMap<String, DiscoveredPeer> =
        std::collections::HashMap::new();
    let mut next_query = Instant::now() + Duration::from_secs(30);
    let mut next_interfaces = Instant::now() + Duration::from_secs(5);
    let mut restarting = false;
    loop {
        if tx.is_closed() {
            return;
        }
        let network_event = monitor.try_iter().any(|ev| {
            matches!(
                ev,
                mdns_sd::DaemonEvent::IpAdd(_) | mdns_sd::DaemonEvent::IpDel(_)
            )
        });
        let now = Instant::now();
        if network_event || now >= next_interfaces {
            if interfaces.refresh(&daemon).unwrap_or(false) {
                next_query = next_query.min(now + Duration::from_secs(1));
            }
            next_interfaces = now + Duration::from_secs(5);
        }
        // mdns-sd doubles browse intervals up to an hour. Restart just the
        // query subscription to cap that at 30s, preserving the cache and
        // debouncing LAN address changes. Never spawn additional browsers.
        if now >= next_query && !restarting {
            if daemon.stop_browse(SERVICE_TYPE).is_err() {
                return;
            }
            restarting = true;
        }
        let ev = match raw_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(ev) => ev,
            Err(_) if raw_rx.is_disconnected() => return,
            Err(_) => continue,
        };
        let to_send = match ev {
            ServiceEvent::SearchStopped(_) if restarting => {
                raw_rx = match daemon.browse(SERVICE_TYPE) {
                    Ok(rx) => rx,
                    Err(_) => return,
                };
                next_query = Instant::now() + Duration::from_secs(30);
                restarting = false;
                None
            }
            ServiceEvent::ServiceResolved(info) => {
                let Some(peer) = peer_from(&info) else {
                    continue;
                };
                if peer.machine_id == own_id {
                    continue;
                }
                if full_to_peer.get(info.get_fullname()) == Some(&peer) {
                    continue;
                }
                full_to_peer.insert(info.get_fullname().to_string(), peer.clone());
                Some(DiscoveryEvent::Found(peer))
            }
            ServiceEvent::ServiceRemoved(_kind, full_name) => full_to_peer
                .remove(&full_name)
                .filter(|peer| {
                    !full_to_peer
                        .values()
                        .any(|other| other.machine_id == peer.machine_id)
                })
                .map(|peer| DiscoveryEvent::Lost {
                    machine_id: peer.machine_id,
                }),
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

    // Stable ordering prevents interface enumeration churn. Retain every
    // candidate so dialers can recover from an unreachable NIC/VPN address.
    let mut addresses: Vec<_> = info
        .get_addresses()
        .iter()
        .copied()
        .filter(|ip| {
            !ip.is_loopback()
                && !matches!(ip, std::net::IpAddr::V6(v6) if v6.is_unicast_link_local())
        })
        .collect();
    addresses.sort_by_key(|ip| (ip.is_loopback(), ip.is_ipv6(), *ip));
    let addresses: Vec<String> = addresses.into_iter().map(|ip| ip.to_string()).collect();
    let host = addresses
        .first()
        .cloned()
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
    // TXT `audio_port` = the Synbad daemon's audio-bridge signaling port.
    // Absent / zero when the peer hasn't opted into audio; the supervisor
    // skips outbound audio dial in that case.
    let audio_port = txt
        .get_property_val_str("audio_port")
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let config_head = txt.get_property_val_str("cfg").unwrap_or("").to_string();

    Some(DiscoveredPeer {
        machine_id,
        display_name,
        host,
        addresses,
        service_port: info.get_port(),
        core_port,
        sync_port,
        audio_port,
        fingerprint,
        protocol_version,
        config_head,
    })
}
