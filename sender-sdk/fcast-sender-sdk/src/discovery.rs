use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context;
use log::debug;
use mdns_sd::ServiceEvent;
use tokio_stream::StreamExt;

use crate::casting_device::CastingDeviceInfo;
use crate::DeviceDiscovererEventHandler;
use crate::IpAddr;

#[cfg(feature = "chromecast")]
pub const CHROMECAST_FRIENDLY_NAME_TXT: &str = "fn";
#[cfg(feature = "fcast")]
pub const FCAST_MDNS_SERVICE_NAME: &str = "_fcast._tcp.local.";
#[cfg(feature = "fcast")]
pub const FASTCAST_MDNS_SERVICE_NAME: &str = "_fastcast._tcp.local.";
#[cfg(feature = "chromecast")]
pub const CHROMECAST_MDNS_SERVICE_NAME: &str = "_googlecast._tcp.local.";
#[cfg(any(feature = "airplay1", feature = "airplay2"))]
pub const AIRPLAY_MDNS_SERVICE_NAME: &str = "_airplay._tcp.local.";

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
    } else if let Some(stripped) = name.strip_suffix(&format!(".{FASTCAST_MDNS_SERVICE_NAME}")) {
        name = stripped.to_string();
    }
    let addresses = std_ip_to_custom(service_info.get_addresses());
    let port = service_info.get_port();
    let device_info = CastingDeviceInfo::fcast(name, addresses, port);
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
    #[cfg(any(feature = "airplay1", feature = "airplay2"))]
    AirPlayServiceEvent(ServiceEvent),
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
    #[cfg(feature = "fcast")]
    let fastcast_mdns_receiver = browse!(service_daemon, FASTCAST_MDNS_SERVICE_NAME)?;
    #[cfg(feature = "chromecast")]
    let chromecast_mdns_receiver = browse!(service_daemon, CHROMECAST_MDNS_SERVICE_NAME)?;
    #[cfg(any(feature = "airplay1", feature = "airplay2"))]
    let airplay_mdns_receiver = browse!(service_daemon, AIRPLAY_MDNS_SERVICE_NAME)?;

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
        (fcast_mdns_receiver, fastcast_mdns_receiver),
        |(fcast_mdns_receiver, fastcast_mdns_receiver): (
            mdns_sd::Receiver<ServiceEvent>,
            mdns_sd::Receiver<ServiceEvent>,
        )| async move {
            tokio::select! {
                fcast = fcast_mdns_receiver.recv_async() => Some((
                    Message::FCastServiceEvent(fcast.ok()?),
                    (fcast_mdns_receiver, fastcast_mdns_receiver)
                )),
                fastcast = fastcast_mdns_receiver.recv_async() => Some((
                    Message::FCastServiceEvent(fastcast.ok()?),
                    (fcast_mdns_receiver, fastcast_mdns_receiver)
                )),
            }
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

    #[cfg(any(feature = "airplay1", feature = "airplay2"))]
    let airplay_mdns_stream = futures::stream::unfold(
        airplay_mdns_receiver,
        |airplay_mdns_receiver: mdns_sd::Receiver<ServiceEvent>| async move {
            let event = airplay_mdns_receiver.recv_async().await.ok()?;
            Some((Message::AirPlayServiceEvent(event), airplay_mdns_receiver))
        },
    );
    #[cfg(any(feature = "airplay1", feature = "airplay2"))]
    tokio::pin!(airplay_mdns_stream);
    #[cfg(any(feature = "airplay1", feature = "airplay2"))]
    #[allow(unused_mut)]
    let mut msg_stream = msg_stream.merge(airplay_mdns_stream);

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
                    let device_info = CastingDeviceInfo::chromecast(name, addresses, port);
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
            #[cfg(any(feature = "airplay1", feature = "airplay2"))]
            Message::AirPlayServiceEvent(service_event) => {
                handle_service_event!(service_event, |service_info: mdns_sd::ServiceInfo| {
                    debug!("Receiver added: {service_info:?}");
                    let fullname = service_info.get_fullname().to_string();
                    let mut name = fullname.clone();
                    if let Some(stripped) =
                        name.strip_suffix(&format!(".{AIRPLAY_MDNS_SERVICE_NAME}"))
                    {
                        name = stripped.to_string();
                    }

                    let is_airplay_2 = if let Some(Some(Ok(vers))) = service_info
                        .get_property("srcvers")
                        .map(|r| r.val_str())
                        .map(|srcvers| srcvers.split('.').nth(0).map(|v| v.parse::<u32>()))
                    {
                        vers >= 200
                    } else {
                        false
                    };

                    let addresses = std_ip_to_custom(service_info.get_addresses());
                    let port = service_info.get_port();

                    let device_info = if is_airplay_2 {
                        CastingDeviceInfo::airplay2(name, addresses, port)
                    } else {
                        CastingDeviceInfo::airplay1(name, addresses, port)
                    };

                    if devices.contains(&fullname) {
                        event_handler.device_changed(device_info);
                    } else {
                        debug!("New AirPlay device `{}`", device_info.name);
                        event_handler.device_available(device_info);
                        devices.insert(fullname);
                    }
                })
            }
        }
    }

    Ok(())
}
