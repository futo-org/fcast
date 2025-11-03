use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Context;
use log::debug;
use mdns_sd::{ScopedIp, ServiceEvent};
use tokio_stream::StreamExt;

use crate::device::DeviceInfo;
use crate::{DeviceDiscovererEventHandler, IpAddr};

#[cfg(feature = "chromecast")]
pub const CHROMECAST_FRIENDLY_NAME_TXT: &str = "fn";
#[cfg(feature = "fcast")]
pub const FCAST_MDNS_SERVICE_NAME: &str = "_fcast._tcp.local.";
#[cfg(feature = "chromecast")]
pub const CHROMECAST_MDNS_SERVICE_NAME: &str = "_googlecast._tcp.local.";

fn strip_service_name(fullname: &str, service_name: &str) -> String {
    if let Some(stripped) = fullname.strip_suffix(&format!(".{service_name}")) {
        stripped.to_string()
    } else {
        fullname.to_string()
    }
}

fn scoped_ip_to_custom(addrs: &HashSet<ScopedIp>) -> Vec<IpAddr> {
    addrs
        .iter()
        .map(|addr| match addr {
            ScopedIp::V4(v4) => IpAddr::from(std::net::IpAddr::V4(*v4.addr())),
            ScopedIp::V6(v6) => {
                let addr = v6.addr();
                let octets = addr.octets();
                IpAddr::V6 {
                    o1: octets[0],
                    o2: octets[1],
                    o3: octets[2],
                    o4: octets[3],
                    o5: octets[4],
                    o6: octets[5],
                    o7: octets[6],
                    o8: octets[7],
                    o9: octets[8],
                    o10: octets[9],
                    o11: octets[10],
                    o12: octets[11],
                    o13: octets[12],
                    o14: octets[13],
                    o15: octets[14],
                    o16: octets[15],
                    scope_id: v6.scope_id().index,
                }
            }
            _ => IpAddr::from(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)), // NOTE: this case will most likely never be hit
        })
        .collect()
}

fn service_resolved(
    devices: &mut HashMap<String, String>,
    event_handler: &Arc<dyn DeviceDiscovererEventHandler>,
    service_info: mdns_sd::ServiceInfo,
    mut device_info: DeviceInfo,
) {
    debug!("Receiver added: {service_info:?}");
    let port = service_info.get_port();
    let resolved_service = service_info.as_resolved_service();
    let addresses = scoped_ip_to_custom(&resolved_service.addresses);
    device_info.port = port;
    device_info.addresses = addresses;
    let fullname = resolved_service.fullname;

    if let std::collections::hash_map::Entry::Vacant(entry) = devices.entry(fullname) {
        debug!("New device `{}`", device_info.name);
        event_handler.device_available(device_info.clone());
        entry.insert(device_info.name);
    } else {
        debug!("Updating device `{}`", device_info.name);
        event_handler.device_changed(device_info);
    }
}

enum Message {
    #[cfg(feature = "fcast")]
    FCastServiceEvent(ServiceEvent),
    #[cfg(feature = "chromecast")]
    ChromecastServiceEvent(ServiceEvent),
}

pub(crate) async fn discover_devices(
    event_handler: Arc<dyn DeviceDiscovererEventHandler>,
) -> anyhow::Result<()> {
    let service_daemon =
        mdns_sd::ServiceDaemon::new().context("Failed to create mDNS ServiceDaemon")?;
    let mut devices: HashMap<String, String> = HashMap::new();

    macro_rules! browse {
        ($mdns:expr, $service:expr) => {
            $mdns
                .browse($service)
                .context(format!("Failed to browse `{}`", $service))
        };
    }

    #[cfg(feature = "fcast")]
    let fcast_mdns_receiver = browse!(service_daemon, FCAST_MDNS_SERVICE_NAME)?;
    #[cfg(feature = "chromecast")]
    let chromecast_mdns_receiver = browse!(service_daemon, CHROMECAST_MDNS_SERVICE_NAME)?;

    let msg_stream = futures::stream::unfold((), async |_| None::<(Message, ())>);
    tokio::pin!(msg_stream);

    #[cfg(feature = "fcast")]
    let fcast_mdns_stream = futures::stream::unfold(
        fcast_mdns_receiver,
        |fcast_mdns_receiver: mdns_sd::Receiver<ServiceEvent>| async move {
            let event = fcast_mdns_receiver.recv_async().await.ok()?;
            Some((Message::FCastServiceEvent(event), fcast_mdns_receiver))
        },
    );
    #[cfg(feature = "fcast")]
    tokio::pin!(fcast_mdns_stream);
    #[cfg(feature = "fcast")]
    #[allow(unused_mut)]
    let mut msg_stream = msg_stream.merge(fcast_mdns_stream);

    #[cfg(feature = "chromecast")]
    let chromecast_mdns_stream = futures::stream::unfold(
        chromecast_mdns_receiver,
        |chromecast_mdns_receiver: mdns_sd::Receiver<ServiceEvent>| async move {
            let event = chromecast_mdns_receiver.recv_async().await.ok()?;
            Some((
                Message::ChromecastServiceEvent(event),
                chromecast_mdns_receiver,
            ))
        },
    );
    #[cfg(feature = "chromecast")]
    tokio::pin!(chromecast_mdns_stream);
    #[cfg(feature = "chromecast")]
    #[allow(unused_mut)]
    let mut msg_stream = msg_stream.merge(chromecast_mdns_stream);

    while let Some(msg) = msg_stream.next().await {
        match msg {
            #[cfg(feature = "fcast")]
            Message::FCastServiceEvent(service_event) => match service_event {
                ServiceEvent::ServiceResolved(service_info) => {
                    let name =
                        strip_service_name(service_info.get_fullname(), FCAST_MDNS_SERVICE_NAME);
                    let device_info = DeviceInfo::fcast(name.clone(), vec![], 0);
                    service_resolved(&mut devices, &event_handler, service_info, device_info);
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    if let Some(name) = devices.remove(&fullname) {
                        event_handler.device_removed(name);
                    } else {
                        debug!("Service `{fullname}` was removed but no device was found");
                    }
                }
                _ => (),
            },
            #[cfg(feature = "chromecast")]
            Message::ChromecastServiceEvent(service_event) => match service_event {
                ServiceEvent::ServiceResolved(service_info) => {
                    let name = service_info
                        .get_property(CHROMECAST_FRIENDLY_NAME_TXT)
                        .map(|name| name.val_str().to_string())
                        .unwrap_or(service_info.get_fullname().to_string());
                    let device_info = DeviceInfo::chromecast(name.clone(), vec![], 0);
                    service_resolved(&mut devices, &event_handler, service_info, device_info);
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    if let Some(name) = devices.remove(&fullname) {
                        event_handler.device_removed(name);
                    } else {
                        debug!("Service `{fullname}` was removed but no device was found");
                    }
                }
                _ => (),
            },
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_service_name() {
        assert_eq!(
            &strip_service_name(
                &format!("FCast-receiver.{FCAST_MDNS_SERVICE_NAME}"),
                FCAST_MDNS_SERVICE_NAME
            ),
            "FCast-receiver"
        );
        assert_eq!(
            &strip_service_name("FCast-receiver", FCAST_MDNS_SERVICE_NAME),
            "FCast-receiver"
        );
    }

    #[test]
    fn test_device_discovery_events() {
        struct TestingEventHandler {}

        impl DeviceDiscovererEventHandler for TestingEventHandler {
            fn device_available(&self, _device_info: DeviceInfo) {}
            fn device_removed(&self, _device_name: String) {}
            fn device_changed(&self, _device_info: DeviceInfo) {}
        }

        let handler: Arc<dyn DeviceDiscovererEventHandler> = Arc::new(TestingEventHandler {});
        let mut devices: HashMap<String, String> = HashMap::new();

        let fcast_dev_info = DeviceInfo::fcast("FCast-receiver".to_string(), vec![], 0);
        service_resolved(
            &mut devices,
            &handler,
            mdns_sd::ServiceInfo::new(
                FCAST_MDNS_SERVICE_NAME,
                "FCast-receiver",
                "",
                (),
                1234,
                Vec::new(),
            )
            .unwrap(),
            fcast_dev_info,
        );

        assert_eq!(
            devices
                .get(&format!("FCast-receiver.{FCAST_MDNS_SERVICE_NAME}"))
                .unwrap(),
            "FCast-receiver"
        );

        let chromecast_dev_info = DeviceInfo::fcast("Chromecast-receiver".to_string(), vec![], 0);
        service_resolved(
            &mut devices,
            &handler,
            mdns_sd::ServiceInfo::new(
                CHROMECAST_MDNS_SERVICE_NAME,
                "Chromecast-abcdefghijklmnopqrstuvwxyz",
                "",
                (),
                1234,
                Vec::new(),
            )
            .unwrap(),
            chromecast_dev_info,
        );

        assert_eq!(
            devices
                .get(&format!(
                    "Chromecast-abcdefghijklmnopqrstuvwxyz.{CHROMECAST_MDNS_SERVICE_NAME}"
                ))
                .unwrap(),
            "Chromecast-receiver"
        );
    }
}
