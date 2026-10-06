use std::collections::HashMap;

use anyhow::Result;
use if_addrs::get_if_addrs;
use mdns_sd::ServiceDaemon;
use tracing::error;

use crate::Mdns;
#[cfg(feature = "google-cast")]
use crate::{GCAST_TCP_PORT, gcast};
#[cfg(feature = "raop")]
use crate::{message::Raop, raop};
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
#[cfg(feature = "google-cast")]
pub fn chromecast_device_name() -> String {
    format!("Chromecast-{}", hostname())
}

/// `s` cut to at most `max` bytes on a char boundary. A DNS label is 63
/// bytes and a TXT string 255 with its key, a longer name is refused by the
/// responder.
fn clip_utf8(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

const MAX_LABEL: usize = 63;

fn is_link_local_v6(ip: std::net::IpAddr) -> bool {
    matches!(ip, std::net::IpAddr::V6(v6) if v6.segments()[0] & 0xffc0 == 0xfe80)
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
    let name = clip_utf8(name, MAX_LABEL);
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
    msg_tx.mdns(Mdns::NameSet(fcast_name.clone()));

    let ifaces = get_if_addrs();
    let mut set_ips_msg = None;

    let daemon = mdns_sd::ServiceDaemon::new()?;
    let monitor = daemon.monitor()?;

    // A sender that connects over a link-local IPv6 address serves its files
    // at a URL that needs a zone no sender writes and no URL parser keeps, so
    // the receiver can never fetch them. IPv4 and global IPv6 stay.
    let link_local_v6 = mdns_sd::IfPredicate::new(|iface| is_link_local_v6(iface.ip()));
    if let Err(err) = daemon.disable_interface(mdns_sd::IfKind::Predicate(link_local_v6)) {
        error!(?err, "Failed to stop advertising link-local IPv6");
    }

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

    #[cfg(feature = "google-cast")]
    if settings.google_cast_enabled() {
        let chromecast_name = settings.chromecast_name();
        let gcast_props = HashMap::from([
            ("fn".to_owned(), clip_utf8(&chromecast_name, 250).to_owned()),
            ("ca".to_owned(), "1".to_owned()), // Has display
        ]);

        // one protocol failing to advertise must not take the others down
        match mdns_sd::ServiceInfo::new(
            "_googlecast._tcp.local.",
            &gcast::get_host_name(&chromecast_name),
            &format!("{}.local.", uuid::Uuid::new_v4()),
            (), // Auto
            GCAST_TCP_PORT,
            gcast_props,
        )
        .map_err(anyhow::Error::from)
        .and_then(|service| Ok(daemon.register(service.enable_addr_auto())?))
        {
            Ok(()) => {}
            Err(err) => error!(?err, "Google Cast not advertised"),
        }
    }

    #[cfg(feature = "raop")]
    if settings.raop_enabled() {
        // one protocol failing to advertise must not take the others down
        match raop::service_info(settings.raop_name())
            .and_then(|(service, config)| Ok((daemon.register(service)?, config)))
        {
            Ok((_, raop_config)) => msg_tx.raop(Raop::ConfigAvailable(raop_config)),
            Err(err) => tracing::error!(?err, "RAOP not advertised"),
        }
    }

    #[cfg(feature = "airplay")]
    if settings.airplay_enabled() {
        match airplay::service_info(fcast_name)
            .and_then(|(service, config)| Ok((daemon.register(service)?, config)))
        {
            Ok((_, airplay_config)) => msg_tx.airplay(AirPlay::ConfigAvailable(airplay_config)),
            Err(err) => tracing::error!(?err, "AirPlay not advertised"),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_clipped_on_a_char_boundary() {
        assert_eq!(clip_utf8("Living room", MAX_LABEL), "Living room");
        let long = "ø".repeat(100);
        let clipped = clip_utf8(&long, MAX_LABEL);
        assert!(clipped.len() <= MAX_LABEL);
        assert_eq!(clipped.len(), 62);
    }

    #[test]
    fn only_link_local_v6_is_filtered() {
        for ll in ["fe80::1", "fe80::d9b7:7c8e:a3b2:a07", "febf::1"] {
            assert!(is_link_local_v6(ll.parse().unwrap()), "{ll}");
        }
        for keep in ["192.168.1.2", "169.254.1.1", "2001:db8::1", "fd00::1", "::1", "fec0::1"] {
            assert!(!is_link_local_v6(keep.parse().unwrap()), "{keep}");
        }
    }

    #[test]
    fn a_long_name_still_builds_a_service() {
        let name = clip_utf8(&"x".repeat(300), MAX_LABEL).to_owned();
        assert!(
            mdns_sd::ServiceInfo::new("_fcast._tcp.local.", &name, &format!("{name}.local."), (), 46899, HashMap::new())
                .is_ok()
        );
    }
}
