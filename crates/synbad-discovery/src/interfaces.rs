use mdns_sd::ServiceDaemon;
use std::collections::HashSet;
use std::net::IpAddr;

#[derive(Default)]
pub(crate) struct InterfacePolicy {
    initialized: bool,
    enabled: HashSet<String>,
    addresses: HashSet<(String, IpAddr)>,
}

fn excluded(name: &str, macos: bool) -> bool {
    name == "lo"
        || name == "lo0"
        || (macos && (name.starts_with("utun") || name == "awdl0" || name == "llw0"))
}

impl InterfacePolicy {
    pub(crate) fn advertised_addresses(&self) -> Vec<IpAddr> {
        let mut addresses: Vec<_> = self.addresses.iter().map(|(_, ip)| *ip).collect();
        addresses.sort();
        addresses.dedup();
        addresses
    }

    /// Apply exclusions again when interfaces appear, including new utun
    /// devices. Return changes to LAN addresses only, so VPN churn cannot
    /// turn discovery into an unbounded query loop.
    pub(crate) fn refresh(&mut self, daemon: &ServiceDaemon) -> Result<bool, mdns_sd::Error> {
        if !self.initialized {
            // Default-deny also covers utun devices created between this
            // refresh and mdns-sd's own interface scan. Only LAN names below
            // are enabled, and selectors persist across interface removal.
            daemon.disable_interface(mdns_sd::IfKind::All)?;
            self.initialized = true;
        }
        let Ok(interfaces) = if_addrs::get_if_addrs() else {
            return Ok(false);
        };
        let mut addresses = HashSet::new();
        for interface in interfaces {
            if excluded(&interface.name, cfg!(target_os = "macos")) || interface.is_loopback() {
                continue;
            } else {
                if !self.enabled.contains(&interface.name) {
                    daemon.enable_interface(interface.name.as_str())?;
                    self.enabled.insert(interface.name.clone());
                }
                addresses.insert((interface.name, interface.addr.ip()));
            }
        }
        let changed = self.addresses != addresses;
        self.addresses = addresses;
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn macos_discovery_excludes_virtual_and_peer_to_peer_interfaces() {
        for name in ["utun0", "utun27", "awdl0", "llw0", "lo0"] {
            assert!(excluded(name, true));
        }
        for name in ["en0", "en1", "bridge0"] {
            assert!(!excluded(name, true));
        }
        assert!(!excluded("utun0", false));
    }
}
