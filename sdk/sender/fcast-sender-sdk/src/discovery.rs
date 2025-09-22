use std::collections::HashSet;
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

#[cfg(feature = "fcast")]
fn handle_fcast_mdns_resolved(
    devices: &mut HashSet<String>,
    service_info: mdns_sd::ServiceInfo,
    event_handler: &Arc<dyn DeviceDiscovererEventHandler>,
) {
    debug!("Receiver added: {service_info:?}");
    let mut name = service_info.get_fullname().to_string();
    if let Some(stripped) = name.strip_suffix(&format!(".{FCAST_MDNS_SERVICE_NAME}")) {
        name = stripped.to_string();
    }
    let port = service_info.get_port();
    let resolved_service = service_info.as_resolved_service();
    let addresses = scoped_ip_to_custom(&resolved_service.addresses);
    let device_info = DeviceInfo::fcast(name, addresses, port);
    let fullname = resolved_service.fullname;
    if devices.contains(&fullname) {
        debug!("Updating FCast device `{}`", device_info.name);
        event_handler.device_changed(device_info);
    } else {
        debug!("New FCast device `{}`", device_info.name);
        event_handler.device_available(device_info);
        devices.insert(fullname);
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

enum Message {
    #[cfg(feature = "fcast")]
    FCastServiceEvent(ServiceEvent),
    #[cfg(feature = "chromecast")]
    ChromecastServiceEvent(ServiceEvent),
}

pub(crate) async fn discover_devices(event_handler: Arc<dyn DeviceDiscovererEventHandler>) -> anyhow::Result<()> {
    let service_daemon = mdns_sd::ServiceDaemon::new().context("Failed to create mDNS ServiceDaemon")?;
    let mut devices: HashSet<String> = HashSet::new();

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

    macro_rules! handle_service_event {
        ($event:expr, $on_resolved:expr) => {
            match $event {
                mdns_sd::ServiceEvent::ServiceResolved(service_info) => $on_resolved(service_info),
                mdns_sd::ServiceEvent::ServiceRemoved(_, fullname) => {
                    if devices.remove(&fullname) {
                        event_handler.device_removed(fullname);
                    } else {
                        debug!("Service `{fullname}` was removed but no device was found");
                    }
                }
                _ => (),
            }
        };
    }

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
            Some((Message::ChromecastServiceEvent(event), chromecast_mdns_receiver))
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
            Message::FCastServiceEvent(service_event) => {
                handle_service_event!(service_event, |service_info: mdns_sd::ServiceInfo| {
                    handle_fcast_mdns_resolved(&mut devices, service_info, &event_handler);
                })
            }
            #[cfg(feature = "chromecast")]
            Message::ChromecastServiceEvent(service_event) => {
                handle_service_event!(service_event, |service_info: mdns_sd::ServiceInfo| {
                    let name = service_info
                        .get_property(CHROMECAST_FRIENDLY_NAME_TXT)
                        .map(|name| name.val_str().to_string())
                        .unwrap_or(service_info.get_fullname().to_string());
                    let port = service_info.get_port();
                    let resolved_service = service_info.as_resolved_service();
                    let addresses = scoped_ip_to_custom(&resolved_service.addresses);
                    let device_info = DeviceInfo::chromecast(name, addresses, port);
                    let fullname = resolved_service.fullname;
                    if devices.contains(&fullname) {
                        debug!("Updating Chromecast device `{}`", device_info.name);
                        event_handler.device_changed(device_info);
                    } else {
                        debug!("New Chromecast device `{}`", device_info.name);
                        event_handler.device_available(device_info);
                        devices.insert(fullname);
                    }
                })
            }
        }
    }

    Ok(())
}
