use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use mdns_sd::{DaemonEvent, IfKind, ServiceDaemon, ServiceInfo};
use pcvc_protocol::PROTOCOL_VERSION;

use crate::{MdnsStatus, Shared};

pub const SERVICE_TYPE: &str = "_pcvolumecontrol._tcp.local.";
/// Version of the TXT record layout, not of the protocol.
const TXT_VERSION: &str = "1";
const DAEMON_REPLY_TIMEOUT: Duration = Duration::from_secs(2);

/// Advertises the service until the server is cancelled, then withdraws it.
pub(crate) async fn run(shared: Arc<Shared>, bind: IpAddr, port: u16) {
    let registered = tokio::task::spawn_blocking(move || register(bind, port)).await;
    let (daemon, fullname) = match registered {
        Ok(Ok(registered)) => registered,
        Ok(Err(e)) => return fail(&shared, format!("mDNS registration failed: {e}")),
        Err(e) => return fail(&shared, format!("mDNS registration panicked: {e}")),
    };
    tracing::info!(%fullname, "mDNS service registered");
    shared.mdns.send_replace(MdnsStatus::Registered {
        instance: instance_name(&fullname).to_owned(),
    });

    match daemon.monitor() {
        Ok(monitor) => loop {
            tokio::select! {
                () = shared.cancel.cancelled() => break,
                event = monitor.recv_async() => match event {
                    Ok(DaemonEvent::NameChange(change))
                        if change.original.eq_ignore_ascii_case(&fullname) =>
                    {
                        tracing::info!(new_name = %change.new_name, "mDNS name conflict resolved");
                        shared.mdns.send_replace(MdnsStatus::Registered {
                            instance: instance_name(&change.new_name).to_owned(),
                        });
                    }
                    Ok(DaemonEvent::Error(e)) => tracing::warn!("mDNS daemon error: {e}"),
                    Ok(_) => {}
                    Err(_) => {
                        fail(&shared, "mDNS daemon stopped unexpectedly".into());
                        shared.cancel.cancelled().await;
                        break;
                    }
                },
            }
        },
        Err(e) => {
            tracing::warn!("mDNS monitor unavailable: {e}");
            shared.cancel.cancelled().await;
        }
    }

    let _ = tokio::task::spawn_blocking(move || unregister(&daemon, &fullname)).await;
    shared.mdns.send_replace(MdnsStatus::Disabled);
}

fn fail(shared: &Shared, message: String) {
    tracing::warn!("{message}");
    shared.mdns.send_replace(MdnsStatus::Failed(message));
}

fn register(bind: IpAddr, port: u16) -> Result<(ServiceDaemon, String), mdns_sd::Error> {
    let host = short_hostname();
    let instance = format!("{host}-pcvolumecontrol");
    let host_name = format!("{host}.local.");
    let protocol = PROTOCOL_VERSION.to_string();
    let properties = [("version", TXT_VERSION), ("protocol", protocol.as_str())];

    let daemon = ServiceDaemon::new()?;
    let info = if bind.is_unspecified() {
        let mut info = ServiceInfo::new(
            SERVICE_TYPE,
            &instance,
            &host_name,
            "",
            port,
            &properties[..],
        )?
        .enable_addr_auto();
        // An IPv4 listener must not be advertised with IPv6 addresses, or
        // clients that pick the AAAA record cannot connect.
        if bind.is_ipv4() {
            info.set_interfaces(vec![IfKind::IPv4]);
        }
        info
    } else {
        let mut info = ServiceInfo::new(
            SERVICE_TYPE,
            &instance,
            &host_name,
            bind,
            port,
            &properties[..],
        )?;
        info.set_interfaces(vec![IfKind::Addr(bind)]);
        info
    };
    let fullname = info.get_fullname().to_owned();
    if let Err(e) = daemon.register(info) {
        let _ = daemon.shutdown();
        return Err(e);
    }
    Ok((daemon, fullname))
}

fn unregister(daemon: &ServiceDaemon, fullname: &str) {
    match daemon.unregister(fullname) {
        Ok(reply) => match reply.recv_timeout(DAEMON_REPLY_TIMEOUT) {
            Ok(status) => tracing::info!(%fullname, ?status, "mDNS service unregistered"),
            Err(e) => tracing::warn!("mDNS unregister did not complete: {e}"),
        },
        Err(e) => tracing::warn!("mDNS unregister failed: {e}"),
    }
    match daemon.shutdown() {
        Ok(reply) => {
            let _ = reply.recv_timeout(DAEMON_REPLY_TIMEOUT);
        }
        Err(e) => tracing::warn!("mDNS daemon shutdown failed: {e}"),
    }
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
