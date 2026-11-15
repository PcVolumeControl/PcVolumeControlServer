//! Needs a network where multicast works, so it is ignored by default:
//! `cargo test -p pcvc-server --test mdns -- --ignored`.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent};
use pcvc_audio::{AudioBackend, MockBackend};
use pcvc_server::{MDNS_SERVICE_TYPE, MdnsStatus, ServerConfig, ServerHandle};
use tokio::sync::broadcast;
use tokio::time::timeout;

mod common;
use common::NoIcons;

const TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires multicast networking"]
async fn advertises_while_running_and_withdraws_on_stop() {
    let config = ServerConfig::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0).with_details(true);
    advertise_and_withdraw(config).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires multicast networking"]
async fn the_txt_record_is_the_same_with_an_icon_provider() {
    let config = ServerConfig::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
        .with_icons(Arc::new(NoIcons(broadcast::Sender::new(1))));
    advertise_and_withdraw(config).await;
}

async fn advertise_and_withdraw(config: ServerConfig) {
    let backend: Arc<dyn AudioBackend> = Arc::new(MockBackend::demo());
    let server = ServerHandle::start(backend, config).await.unwrap();
    let port = server.local_addr().port();

    let mut status = server.mdns();
    let instance = timeout(
        TIMEOUT,
        status.wait_for(|s| matches!(s, MdnsStatus::Registered { .. })),
    )
    .await
    .expect("not registered")
    .unwrap()
    .clone();
    let MdnsStatus::Registered { instance } = instance else {
        unreachable!()
    };
    assert!(instance.contains("-pcvolumecontrol"), "{instance}");

    let browser = ServiceDaemon::new().unwrap();
    let events = browser.browse(MDNS_SERVICE_TYPE).unwrap();
    let fullname = timeout(TIMEOUT, async {
        loop {
            if let ServiceEvent::ServiceResolved(service) = events.recv_async().await.unwrap()
                && service.port == port
            {
                assert_eq!(service.get_property_val_str("version"), Some("1"));
                assert_eq!(service.get_property_val_str("protocol"), Some("8"));
                assert_eq!(service.get_property_val_str("features"), None);
                assert_eq!(service.get_properties().len(), 2);
                // Other responders for the same host (avahi, for one) may add
                // IPv6 addresses, so only check that an IPv4 one is present.
                assert!(service.addresses.iter().any(|a| a.to_ip_addr().is_ipv4()));
                return service.fullname.clone();
            }
        }
    })
    .await
    .expect("service not discovered");
    assert!(fullname.starts_with(&instance), "{fullname}");

    timeout(TIMEOUT, server.stop()).await.expect("stop hung");
    timeout(TIMEOUT, async {
        loop {
            if let ServiceEvent::ServiceRemoved(_, name) = events.recv_async().await.unwrap()
                && name == fullname
            {
                return;
            }
        }
    })
    .await
    .expect("service not withdrawn");
    assert_eq!(*status.borrow(), MdnsStatus::Disabled);
    let _ = browser.shutdown();
}
