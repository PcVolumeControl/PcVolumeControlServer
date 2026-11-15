use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use mdns_sd::{DaemonEvent, IfKind, Receiver, ServiceDaemon, ServiceInfo};
use pcvc_protocol::PROTOCOL_VERSION;

use crate::{MdnsStatus, Shared};

pub const SERVICE_TYPE: &str = "_pcvolumecontrol._tcp.local.";
/// Version of the TXT record layout, not of the protocol.
const TXT_VERSION: &str = "1";
const DAEMON_REPLY_TIMEOUT: Duration = Duration::from_secs(2);

/// Shuts the daemon down when dropped, which also sends goodbye packets for
/// the registered service. `ServiceDaemon` has no `Drop` of its own; without
/// this its thread and sockets would outlive the server.
struct Daemon(Option<ServiceDaemon>);

impl Daemon {
    fn get(&self) -> &ServiceDaemon {
        self.0.as_ref().expect("daemon is present until shut down")
    }

    /// Shuts down and waits (briefly) for the daemon thread to finish.
    fn shutdown_and_wait(mut self) {
        let Some(daemon) = self.0.take() else { return };
        match daemon.shutdown() {
            Ok(reply) => {
                if let Err(e) = reply.recv_timeout(DAEMON_REPLY_TIMEOUT) {
                    tracing::warn!("mDNS daemon shutdown did not complete: {e}");
                }
            }
            Err(e) => tracing::warn!("mDNS daemon shutdown failed: {e}"),
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(daemon) = self.0.take() {
            let _ = daemon.shutdown();
        }
    }
}

/// Advertises the service until the server is cancelled, then withdraws it.
///
/// The status is `Starting` until the daemon reports an announcement for the
/// service, and `Failed` whenever it reports an error.
pub(crate) async fn run(shared: Arc<Shared>, bind: IpAddr, port: u16) {
    shared.mdns.send_replace(MdnsStatus::Starting);
    let registered = tokio::task::spawn_blocking(move || register(bind, port)).await;
    let (daemon, monitor, mut fullname) = match registered {
        Ok(Ok(registered)) => registered,
        Ok(Err(e)) => return fail(&shared, format!("mDNS registration failed: {e}")),
        Err(e) => return fail(&shared, format!("mDNS registration panicked: {e}")),
    };
    tracing::debug!(%fullname, "mDNS service registration requested");

    loop {
        tokio::select! {
            () = shared.cancel.cancelled() => break,
            event = monitor.recv_async() => match event {
                Ok(DaemonEvent::Announce(name, _)) if name.eq_ignore_ascii_case(&fullname) => {
                    let instance = instance_name(&fullname).to_owned();
                    let status = MdnsStatus::Registered { instance };
                    if *shared.mdns.borrow() != status {
                        tracing::info!(%fullname, "mDNS service advertised");
                        shared.mdns.send_replace(status);
                    }
                }
                Ok(DaemonEvent::NameChange(change))
                    if change.original.eq_ignore_ascii_case(&fullname) =>
                {
                    tracing::info!(new_name = %change.new_name, "mDNS name conflict resolved");
                    fullname = change.new_name;
                    shared.mdns.send_replace(MdnsStatus::Registered {
                        instance: instance_name(&fullname).to_owned(),
                    });
                }
                Ok(DaemonEvent::Error(e)) => fail(&shared, format!("mDNS error: {e}")),
                Ok(_) => {}
                Err(_) => {
                    fail(&shared, "mDNS daemon stopped unexpectedly".into());
                    shared.cancel.cancelled().await;
                    break;
                }
            },
        }
    }

    let _ = tokio::task::spawn_blocking(move || daemon.shutdown_and_wait()).await;
    tracing::info!(%fullname, "mDNS service withdrawn");
    shared.mdns.send_replace(MdnsStatus::Disabled);
}

fn fail(shared: &Shared, message: String) {
    tracing::warn!("{message}");
    shared.mdns.send_replace(MdnsStatus::Failed(message));
}

fn register(
    bind: IpAddr,
    port: u16,
) -> Result<(Daemon, Receiver<DaemonEvent>, String), mdns_sd::Error> {
    let host = short_hostname();
    let instance = format!("{host}-pcvolumecontrol");
    let host_name = format!("{host}.local.");
    let protocol = PROTOCOL_VERSION.to_string();
    let properties = [("version", TXT_VERSION), ("protocol", protocol.as_str())];

    let addresses: Vec<IpAddr> = if bind.is_unspecified() {
        Vec::new()
    } else {
        vec![bind]
    };
    let mut info = ServiceInfo::new(
        SERVICE_TYPE,
        &instance,
        &host_name,
        &addresses[..],
        port,
        &properties[..],
    )?;
    // Only advertise addresses the listener accepts on. An unspecified IPv6
    // bind may or may not be dual-stack depending on the platform, so it is
    // advertised as IPv6 only.
    let interface = match bind {
        IpAddr::V4(ip) if ip.is_unspecified() => IfKind::IPv4,
        IpAddr::V6(ip) if ip.is_unspecified() => IfKind::IPv6,
        ip => IfKind::Addr(ip),
    };
    info.set_interfaces(vec![interface]);
    if bind.is_unspecified() {
        info = info.enable_addr_auto();
    }
    let fullname = info.get_fullname().to_owned();

    let daemon = Daemon(Some(ServiceDaemon::new()?));
    // Monitor before registering: registration errors are only reported to
    // monitors that already exist.
    let monitor = daemon.get().monitor()?;
    daemon.get().register(info)?;
    Ok((daemon, monitor, fullname))
}

/// The first label of the host name, which is what `.local` names use.
fn short_hostname() -> String {
    let full = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_default();
    match full.split('.').next() {
        Some(label) if !label.is_empty() => label.to_owned(),
        _ => "pcvolumecontrol-host".to_owned(),
    }
}

fn instance_name(fullname: &str) -> &str {
    fullname
        .strip_suffix(SERVICE_TYPE)
        .and_then(|s| s.strip_suffix('.'))
        .unwrap_or(fullname)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_name_strips_service_type() {
        assert_eq!(
            instance_name("box-pcvolumecontrol._pcvolumecontrol._tcp.local."),
            "box-pcvolumecontrol"
        );
        assert_eq!(
            instance_name("box-pcvolumecontrol (2)._pcvolumecontrol._tcp.local."),
            "box-pcvolumecontrol (2)"
        );
        assert_eq!(instance_name("other"), "other");
    }

    #[test]
    fn hostname_is_a_single_label() {
        let host = short_hostname();
        assert!(!host.is_empty());
        assert!(!host.contains('.'));
    }
}
