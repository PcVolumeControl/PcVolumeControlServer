//! Pure reductions from app state to what the tray and views display.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use crate::server_ctl::ServerStatus;
use crate::tray::TrayState;

/// Non-loopback, non-link-local IPv4 addresses of this machine, likeliest
/// home-LAN address first.
pub fn local_ipv4s() -> Vec<Ipv4Addr> {
    let mut addrs: Vec<Ipv4Addr> = match if_addrs::get_if_addrs() {
        Ok(interfaces) => interfaces
            .into_iter()
            .filter_map(|i| match i.ip() {
                IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_link_local() => Some(ip),
                _ => None,
            })
            .collect(),
        Err(e) => {
            tracing::warn!("listing network interfaces failed: {e}");
            Vec::new()
        }
    };
    sort_by_preference(&mut addrs);
    addrs
}

/// Orders 192.168/16 before 10/8 before 172.16/12 (where container bridges
/// usually live) before anything else, then numerically, and deduplicates.
pub fn sort_by_preference(addrs: &mut Vec<Ipv4Addr>) {
    fn rank(ip: &Ipv4Addr) -> u8 {
        match ip.octets() {
            [192, 168, ..] => 0,
            [10, ..] => 1,
            [172, b, ..] if (16..32).contains(&b) => 2,
            _ => 3,
        }
    }
    addrs.sort_by_key(|ip| (rank(ip), *ip));
    addrs.dedup();
}

/// The `address:port` strings a phone can connect to. An unspecified bind
/// address expands to every local IPv4 address (or stays as is when there
/// are none).
pub fn listen_addresses(addr: SocketAddr, local: &[Ipv4Addr]) -> Vec<String> {
    if addr.ip().is_unspecified() && !local.is_empty() {
        local
            .iter()
            .map(|ip| SocketAddr::new(IpAddr::V4(*ip), addr.port()).to_string())
            .collect()
    } else {
        vec![addr.to_string()]
    }
}

pub fn clients_label(count: usize) -> String {
    match count {
        1 => "1 client".to_owned(),
        n => format!("{n} clients"),
    }
}

pub fn tray_state(status: &ServerStatus, clients: usize) -> TrayState {
    match status {
        ServerStatus::Running(_) if clients > 0 => TrayState::ClientsConnected,
        ServerStatus::Running(_) => TrayState::Online,
        _ => TrayState::Offline,
    }
}

/// For example "PcVolumeControl: 192.168.1.20:3000, 2 clients". With several
/// addresses only the first is shown.
pub fn tooltip(status: &ServerStatus, clients: usize, local: &[Ipv4Addr]) -> String {
    match status {
        ServerStatus::Running(addr) => {
            let addresses = listen_addresses(*addr, local);
            let shown = match addresses.as_slice() {
                [one] => one.clone(),
                [first, rest @ ..] => format!("{first} (+{})", rest.len()),
                [] => addr.to_string(),
            };
            format!("PcVolumeControl: {shown}, {}", clients_label(clients))
        }
        ServerStatus::Starting => "PcVolumeControl: starting".to_owned(),
        ServerStatus::Stopped => "PcVolumeControl: stopped".to_owned(),
        ServerStatus::Failed(_) => "PcVolumeControl: server failed".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAN: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 20);
    const VPN: Ipv4Addr = Ipv4Addr::new(10, 8, 0, 2);

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn unspecified_bind_expands_to_local_addresses() {
        assert_eq!(
            listen_addresses(addr("0.0.0.0:3000"), &[VPN, LAN]),
            ["10.8.0.2:3000", "192.168.1.20:3000"]
        );
        assert_eq!(
            listen_addresses(addr("0.0.0.0:3000"), &[]),
            ["0.0.0.0:3000"]
        );
        assert_eq!(
            listen_addresses(addr("192.168.1.20:4000"), &[VPN, LAN]),
            ["192.168.1.20:4000"]
        );
    }

    #[test]
    fn lan_addresses_come_first() {
        let mut addrs = vec![
            Ipv4Addr::new(172, 17, 0, 1),
            Ipv4Addr::new(100, 64, 0, 7),
            VPN,
            LAN,
            Ipv4Addr::new(192, 168, 0, 9),
            LAN,
        ];
        sort_by_preference(&mut addrs);
        assert_eq!(
            addrs,
            [
                Ipv4Addr::new(192, 168, 0, 9),
                LAN,
                VPN,
                Ipv4Addr::new(172, 17, 0, 1),
                Ipv4Addr::new(100, 64, 0, 7),
            ]
        );
    }

    #[test]
    fn tooltips() {
        let running = ServerStatus::Running(addr("0.0.0.0:3000"));
        assert_eq!(
            tooltip(&running, 2, &[LAN]),
            "PcVolumeControl: 192.168.1.20:3000, 2 clients"
        );
        assert_eq!(
            tooltip(&running, 1, &[VPN, LAN]),
            "PcVolumeControl: 10.8.0.2:3000 (+1), 1 client"
        );
        assert_eq!(
            tooltip(&ServerStatus::Stopped, 0, &[LAN]),
            "PcVolumeControl: stopped"
        );
    }

    #[test]
    fn tray_states() {
        let running = ServerStatus::Running(addr("0.0.0.0:3000"));
        assert_eq!(tray_state(&running, 0), TrayState::Online);
        assert_eq!(tray_state(&running, 3), TrayState::ClientsConnected);
        assert_eq!(tray_state(&ServerStatus::Starting, 3), TrayState::Offline);
    }
}
