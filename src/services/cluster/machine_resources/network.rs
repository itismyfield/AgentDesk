//! Select the interface that owns the advertised API address and measure its
//! receive/transmit byte counters. Virtual adapters are never summed together.
use std::collections::HashSet;
use std::net::IpAddr;
use std::time::Instant;

use sysinfo::{NetworkData, Networks};

use super::NetworkResources;

pub(super) fn advertised_ip(api_base_url: &str) -> Option<IpAddr> {
    let url = url::Url::parse(api_base_url).ok()?;
    match url.host()? {
        url::Host::Ipv4(address) => Some(address.into()),
        url::Host::Ipv6(address) => Some(address.into()),
        url::Host::Domain(_) => None,
    }
}

pub(super) struct NetworkSampler {
    networks: Networks,
    advertised_ip: Option<IpAddr>,
    wired_interfaces: HashSet<String>,
    previous: Option<(String, u64, u64, Instant)>,
}

impl NetworkSampler {
    pub(super) fn new(advertised_ip: Option<IpAddr>, wired_interfaces: HashSet<String>) -> Self {
        Self {
            networks: Networks::new_with_refreshed_list(),
            advertised_ip,
            wired_interfaces,
            previous: None,
        }
    }

    pub(super) fn collect(&mut self) -> Option<NetworkResources> {
        self.networks.refresh(true);
        let now = Instant::now();
        let selected = self.select()?;
        let name = selected.0.clone();
        let received = selected.1.total_received();
        let transmitted = selected.1.total_transmitted();
        let elapsed = self
            .previous
            .as_ref()
            .filter(|(previous, _, _, _)| previous == &name);
        let rate = |current: u64, previous: u64, start: Instant| {
            let seconds = now.duration_since(start).as_secs_f64();
            (seconds >= 0.1)
                .then(|| current.checked_sub(previous))
                .flatten()
                .map(|delta| (delta as f64 / seconds).round() as u64)
        };
        let snapshot = NetworkResources {
            wired: self.wired_interfaces.contains(&name) || looks_wired(&name),
            interface: name.clone(),
            received_bytes_per_sec: elapsed
                .and_then(|(_, previous, _, start)| rate(received, *previous, *start)),
            transmitted_bytes_per_sec: elapsed
                .and_then(|(_, _, previous, start)| rate(transmitted, *previous, *start)),
        };
        self.previous = Some((name, received, transmitted, now));
        Some(snapshot)
    }

    fn select(&self) -> Option<(&String, &NetworkData)> {
        if let Some(ip) = self.advertised_ip {
            if let Some(interface) = self
                .networks
                .iter()
                .find(|(_, data)| data.ip_networks().iter().any(|address| address.addr == ip))
            {
                return Some(interface);
            }
        }
        // Without an address match, only a known physical wired interface is
        // eligible. This avoids reporting VPN or Hyper-V traffic as Ethernet.
        self.networks
            .iter()
            .filter(|(name, data)| {
                (self.wired_interfaces.contains(*name) || looks_wired(name))
                    && data
                        .ip_networks()
                        .iter()
                        .any(|address| !address.addr.is_loopback())
            })
            .max_by(|(a_name, a), (b_name, b)| {
                a.received()
                    .saturating_add(a.transmitted())
                    .cmp(&b.received().saturating_add(b.transmitted()))
                    .then_with(|| b_name.cmp(a_name))
            })
    }
}

fn looks_wired(name: &str) -> bool {
    let lower = name.to_lowercase();
    !lower.starts_with("vethernet")
        && (lower.contains("ethernet")
            || lower.contains("이더넷")
            || lower.starts_with("eth")
            || lower.starts_with("enp"))
}

#[cfg(target_os = "macos")]
pub(super) async fn wired_interfaces() -> HashSet<String> {
    let Some(output) =
        super::command::run("/usr/sbin/networksetup", &["-listallhardwareports"]).await
    else {
        return HashSet::new();
    };
    parse_hardware_ports(&String::from_utf8_lossy(&output))
}

#[cfg(not(target_os = "macos"))]
pub(super) async fn wired_interfaces() -> HashSet<String> {
    HashSet::new()
}

#[cfg(any(test, target_os = "macos"))]
fn parse_hardware_ports(raw: &str) -> HashSet<String> {
    raw.split("\n\n")
        .filter_map(|block| {
            let port = block
                .lines()
                .find_map(|line| line.strip_prefix("Hardware Port: "))?;
            let device = block
                .lines()
                .find_map(|line| line.strip_prefix("Device: "))?;
            port.starts_with("Ethernet")
                .then(|| device.trim().to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertised_address_preserves_ipv4_and_ipv6_literals() {
        assert_eq!(
            advertised_ip("http://192.0.2.12:8080"),
            Some("192.0.2.12".parse().unwrap())
        );
        assert_eq!(
            advertised_ip("http://[2001:db8::12]:8080"),
            Some("2001:db8::12".parse().unwrap())
        );
        assert_eq!(advertised_ip("http://host.example:8080"), None);
        assert_eq!(advertised_ip("not a URL"), None);
    }

    #[test]
    fn ethernet_discovery_excludes_wifi_and_virtual_interfaces() {
        let ports = "Hardware Port: Ethernet\nDevice: en0\n\nHardware Port: Wi-Fi\nDevice: en1\n\nHardware Port: Ethernet Adapter (en5)\nDevice: en5\n";
        assert_eq!(
            parse_hardware_ports(ports),
            HashSet::from(["en0".into(), "en5".into()])
        );
        assert!(looks_wired("이더넷"));
        assert!(!looks_wired("vEthernet (Default Switch)"));
        assert!(!looks_wired("Tailscale"));
    }
}
