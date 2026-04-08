use std::collections::HashMap;

use anyhow::Result;
use if_addrs::get_if_addrs;
use mdns_sd::ServiceDaemon;

use crate::{FCAST_TCP_PORT, GCAST_TCP_PORT, Mdns, Raop, gcast, raop};

/// Must be called from a tokio context.
pub fn start_daemon(
    msg_tx: &crate::MessageSender,
    cli_args: &crate::CliArgs,
) -> Result<ServiceDaemon> {
    let host_name = gethostname::gethostname();
    let host_name = host_name.to_string_lossy();
    let device_name = format!("FCast-{host_name}");
    // Avoid naming confusion
    let gcast_device_name = format!("Chromecast-{host_name}");
    msg_tx.mdns(Mdns::NameSet(device_name.clone()));

    if let Ok(ifaces) = get_if_addrs() {
        msg_tx.mdns(Mdns::SetIps(
            ifaces.into_iter().map(|iface| iface.addr.ip()).collect(),
        ));
    }

    let daemon = mdns_sd::ServiceDaemon::new()?;

    let service = mdns_sd::ServiceInfo::new(
        "_fcast._tcp.local.",
        &device_name,
        &format!("{device_name}.local."),
        (), // Auto
        FCAST_TCP_PORT,
        None::<std::collections::HashMap<String, String>>,
    )?
    .enable_addr_auto();

    daemon.register(service)?;

    if !cli_args.no_google_cast {
        let gcast_props = HashMap::from([
            ("fn".to_owned(), gcast_device_name.clone()),
            ("ca".to_owned(), "1".to_owned()), // Has display
        ]);

        let gcast_service = mdns_sd::ServiceInfo::new(
            "_googlecast._tcp.local.",
            &gcast::get_host_name(&gcast_device_name),
            &format!("{}.local.", uuid::Uuid::new_v4()),
            (), // Auto
            GCAST_TCP_PORT,
            gcast_props,
        )?
        .enable_addr_auto();

        daemon.register(gcast_service)?;
    }

    if !cli_args.no_raop {
        let (raop_service, raop_config) = raop::service_info(device_name).unwrap();
        daemon.register(raop_service).unwrap();
        msg_tx.raop(Raop::ConfigAvailable(raop_config));
    }

    let monitor = daemon.monitor()?;
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
