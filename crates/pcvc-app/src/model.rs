//! Pure reductions from app state to what the tray and views display.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Local, TimeZone};

use crate::server_ctl::ServerStatus;
use crate::tray::TrayState;

/// Non-loopback, non-link-local IPv4 addresses of this machine, likeliest
/// home-LAN address first.
pub fn local_ipv4s() -> std::io::Result<Vec<Ipv4Addr>> {
    let mut addrs: Vec<Ipv4Addr> = if_addrs::get_if_addrs()?
        .into_iter()
        .filter_map(|i| match i.ip() {
            IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_link_local() => Some(ip),
            _ => None,
        })
        .collect();
    sort_by_preference(&mut addrs);
    Ok(addrs)
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

/// For example "45s", "3m 05s" or "2h 07m".
pub fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// Wall-clock time as "HH:MM:SS".
pub fn clock<Tz: TimeZone>(time: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    time.format("%H:%M:%S").to_string()
}

/// Local wall-clock time of `since` and how long ago it was, for example
/// "14:03:27 (3m 05s ago)".
pub fn connected_since(since: SystemTime, now: SystemTime) -> String {
    connected_since_in(since, now, &Local)
}

fn connected_since_in<Tz: TimeZone>(since: SystemTime, now: SystemTime, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let local = DateTime::<chrono::Utc>::from(since).with_timezone(tz);
    let ago = now.duration_since(since).unwrap_or_default();
    format!("{} ({} ago)", clock(&local), format_elapsed(ago))
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
    fn elapsed_and_since() {
        assert_eq!(format_elapsed(Duration::from_secs(45)), "45s");
        assert_eq!(format_elapsed(Duration::from_secs(185)), "3m 05s");
        assert_eq!(format_elapsed(Duration::from_secs(7620)), "2h 07m");
        let since = SystemTime::UNIX_EPOCH + Duration::from_secs(14 * 3600 + 3 * 60 + 27);
        let plus_two = chrono::FixedOffset::east_opt(2 * 3600).unwrap();
        assert_eq!(
            connected_since_in(since, since + Duration::from_secs(185), &plus_two),
            "16:03:27 (3m 05s ago)"
        );
        assert_eq!(
            connected_since_in(since, SystemTime::UNIX_EPOCH, &chrono::Utc),
            "14:03:27 (0s ago)"
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
