#[cfg(any_protocol)]
mod any_protocol_prelude {
    pub use anyhow::{anyhow, bail};
    pub use log::{error, info};
    pub use std::{net::SocketAddr, time::Duration};
    pub use tokio::{net::TcpStream, time::sleep};
}

#[cfg(any_protocol)]
use any_protocol_prelude::*;

/// # Arguments
///
///    * on_cmd: return true if the connect loop should quit.
#[cfg(any_protocol)]
pub(crate) async fn try_connect_tcp<T>(
    addrs: Vec<SocketAddr>,
    max_retires: usize,
    cmd_rx: &mut tokio::sync::mpsc::Receiver<T>,
    on_cmd: impl Fn(T) -> bool,
) -> anyhow::Result<Option<tokio::net::TcpStream>> {
    let mut retries = 0;
    loop {
        if retries > max_retires {
            bail!("Exceeded maximum retries ({max_retires})");
        }

        info!("Trying to connect to {addrs:?}...");
        tokio::select! {
            stream = tokio::time::timeout(
                Duration::from_secs(1),
                TcpStream::connect(addrs.as_slice()),
            ) => {
                match stream {
                    Ok(stream) => match stream {
                        Ok(stream) => return Ok(Some(stream)),
                        Err(err) => {
                            error!("Failed to connect: {err}");
                            sleep(Duration::from_secs(1)).await;
                        }
                    },
                    Err(_) => {
                        info!("Failed to connect, retrying...");
                        sleep(Duration::from_secs(1)).await;
                    }
                }
            }
            cmd = cmd_rx.recv() => {
                let cmd = cmd.ok_or(anyhow!("No more commands"))?;
                if on_cmd(cmd) {
                    return Ok(None);
                }
            }
        }

        retries += 1;
    }
}
