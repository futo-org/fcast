use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context;
use log::debug;
use mdns_sd::ServiceEvent;
use tokio_stream::StreamExt;

use crate::device::DeviceInfo;
use crate::DeviceDiscovererEventHandler;
use crate::IpAddr;

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
    let addresses = std_ip_to_custom(service_info.get_addresses());
    let port = service_info.get_port();
    let device_info = DeviceInfo::fcast(name, addresses, port);
    if devices.contains(service_info.get_fullname()) {
        debug!("Updating FCast device `{}`", device_info.name);
        event_handler.device_changed(device_info);
    } else {
        debug!("New FCast device `{}`", device_info.name);
        event_handler.device_available(device_info);
        devices.insert(service_info.get_fullname().to_string());
    }
}

fn std_ip_to_custom(addrs: &HashSet<std::net::IpAddr>) -> Vec<IpAddr> {
    addrs.iter().map(IpAddr::from).collect()
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
                    let addresses = std_ip_to_custom(service_info.get_addresses());
                    let port = service_info.get_port();
                    let device_info = DeviceInfo::chromecast(name, addresses, port);
                    if devices.contains(service_info.get_fullname()) {
                        debug!("Updating Chromecast device `{}`", device_info.name);
                        event_handler.device_changed(device_info);
                    } else {
                        debug!("New Chromecast device `{}`", device_info.name);
                        event_handler.device_available(device_info);
                        devices.insert(service_info.get_fullname().to_string());
                    }
                })
            }
        }
    }

    Ok(())
}
