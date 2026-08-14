use std::collections::HashMap;

use anyhow::Result;
use if_addrs::get_if_addrs;
use mdns_sd::ServiceDaemon;
use tracing::error;

use crate::{GCAST_TCP_PORT, Mdns, Raop, gcast, raop};
#[cfg(feature = "airplay")]
use crate::{airplay, message::AirPlay};

/// The local hostname; also expands the `{hostname}` variable in a configured
/// name.
pub fn hostname() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}

/// The default FCast instance name, `FCast-<hostname>`.
pub fn fcast_device_name() -> String {
    format!("FCast-{}", hostname())
}

/// The default Google Cast display name, `Chromecast-<hostname>`.
pub fn chromecast_device_name() -> String {
    format!("Chromecast-{}", hostname())
}

/// Advertise `_fcast._tcp` under `name`. Call only once the listening port is
/// committed, so a second instance that can't bind the default port never
/// advertises a duplicate record.
pub fn register_fcast(
    daemon: &ServiceDaemon,
    name: &str,
    port: u16,
    fcast_txt_records: &HashMap<String, String>,
) -> Result<()> {
    let service = mdns_sd::ServiceInfo::new(
        "_fcast._tcp.local.",
        name,
        &format!("{name}.local."),
        (), // Auto
        port,
        fcast_txt_records.to_owned(),
    )?
    .enable_addr_auto();
    daemon.register(service)?;

    Ok(())
}

/// Must be called from a tokio context.
#[tracing::instrument(skip_all)]
pub fn start_daemon(
    msg_tx: &crate::MessageSender,
    settings: &crate::Settings,
) -> Result<ServiceDaemon> {
    let fcast_name = settings.fcast_name();
    let raop_name = settings.raop_name();
    let chromecast_name = settings.chromecast_name();
    msg_tx.mdns(Mdns::NameSet(fcast_name.clone()));

    let ifaces = get_if_addrs();
    let mut set_ips_msg = None;

    let daemon = mdns_sd::ServiceDaemon::new()?;
    let monitor = daemon.monitor()?;

    if let Some(excluded_interfaces) = settings.exclude_interfaces() {
        match regex::Regex::new(excluded_interfaces) {
            Ok(re) => {
                if let Ok(ifaces) = &ifaces {
                    set_ips_msg = Some(Mdns::SetIps(
                        ifaces
                            .iter()
                            .filter(|iface| !re.is_match(&iface.name))
                            .map(|iface| iface.addr.ip())
                            .collect(),
                    ))
                }
                let rule = mdns_sd::IfPredicate::new(move |iface| re.is_match(&iface.name));
                if let Err(err) = daemon.disable_interface(mdns_sd::IfKind::Predicate(rule)) {
                    error!(?err, "Failed to disable interface");
                }
            }
            Err(err) => {
                error!(
                    ?err,
                    excluded_interfaces, "Failed to create interface blocklist regex"
                );
            }
        }
    }

    if set_ips_msg.is_none()
        && let Ok(ifaces) = ifaces
    {
        set_ips_msg = Some(Mdns::SetIps(
            ifaces.into_iter().map(|iface| iface.addr.ip()).collect(),
        ));
    }

    if let Some(msg) = set_ips_msg {
        msg_tx.mdns(msg);
    }

    // `_fcast._tcp` is registered later, from `register_fcast`, once the listening
    // port is committed.

    if settings.google_cast_enabled() {
        let gcast_props = HashMap::from([
            ("fn".to_owned(), chromecast_name.clone()),
            ("ca".to_owned(), "1".to_owned()), // Has display
        ]);

        let gcast_service = mdns_sd::ServiceInfo::new(
            "_googlecast._tcp.local.",
            &gcast::get_host_name(&chromecast_name),
            &format!("{}.local.", uuid::Uuid::new_v4()),
            (), // Auto
            GCAST_TCP_PORT,
            gcast_props,
        )?
        .enable_addr_auto();

        daemon.register(gcast_service)?;
    }

    if settings.raop_enabled() {
        let (raop_service, raop_config) = raop::service_info(raop_name).unwrap();
        daemon.register(raop_service).unwrap();
        msg_tx.raop(Raop::ConfigAvailable(raop_config));
    }

    #[cfg(feature = "airplay")]
    if settings.airplay_enabled() {
        let (airplay_service, airplay_config) = airplay::service_info(fcast_name).unwrap();
        daemon.register(airplay_service).unwrap();
        msg_tx.airplay(AirPlay::ConfigAvailable(airplay_config));
    }

    let msg_tx = msg_tx.clone();
    tokio::spawn(async move {
        while let Ok(msg) = monitor.recv_async().await {
            let event = match msg {
                mdns_sd::DaemonEvent::IpAdd(addr) => Mdns::IpAdded(addr),
                mdns_sd::DaemonEvent::IpDel(addr) => Mdns::IpRemoved(addr),
                _ => continue,
            };
            msg_tx.mdns(event);
        }
    });

    Ok(daemon)
}
