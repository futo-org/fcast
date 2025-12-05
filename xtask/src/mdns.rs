use anyhow::Result;
use clap::{Args, Subcommand};

#[derive(Subcommand)]
pub enum MdnsCommand {
    Publish {
        name: String,
        address: String,
        #[clap(default_value = "_fcast._tcp.local.")]
        service_type: String,
    },
}

#[derive(Args)]
pub struct MdnsArgs {
    #[clap(subcommand)]
    pub cmd: MdnsCommand,
}

impl MdnsArgs {
    pub fn run(self) -> Result<()> {
        match self.cmd {
            MdnsCommand::Publish {
                name,
                address,
                service_type,
            } => {
                let mdns = mdns_sd::ServiceDaemon::new()?;
                let service = mdns_sd::ServiceInfo::new(
                    &service_type,
                    &name,
                    &format!("{address}.local."),
                    &address,
                    10000,
                    None,
                )?;

                println!("Publishing service: {service:?}");
                mdns.register(service)?;

                let (tx, rx) = std::sync::mpsc::channel();

                ctrlc::set_handler(move || tx.send(()).unwrap())?;

                println!("Press Ctrl+C to quit");
                rx.recv()?;

                println!("Unpublishing...");
                let unreg_rx = mdns.unregister(&format!("{name}.{service_type}"))?;

                for event in unreg_rx {
                    println!("Unregister event: {event:?}");
                }

                mdns.shutdown()?;
            }
        }

        Ok(())
    }
}
