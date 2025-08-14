#[cfg(any_protocol)]
mod any_protocol_prelude {
    pub use anyhow::{anyhow, bail};
    pub use log::info;
    pub use std::{net::SocketAddr, time::Duration};
    pub use tokio::net::TcpStream;
}

#[cfg(any_protocol)]
use any_protocol_prelude::*;

/// # Arguments
///
///    * on_cmd: return true if the connect loop should quit.
#[cfg(any_protocol)]
pub(crate) async fn try_connect_tcp<T>(
    addrs: Vec<SocketAddr>,
    timeout: Duration,
    cmd_rx: &mut tokio::sync::mpsc::Receiver<T>,
    on_cmd: impl Fn(T) -> bool,
) -> anyhow::Result<Option<tokio::net::TcpStream>> {
    anyhow::ensure!(!addrs.is_empty());

    debug!("Trying to connect to {addrs:?}...");

    let mut connections: Vec<_> = addrs
        .into_iter()
        .map(|addr| Box::pin(tokio::time::timeout(timeout, TcpStream::connect(addr))))
        .collect();

    let (connection_tx, mut connection_rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        'out: {
            while !connections.is_empty() {
                match futures::future::select_all(connections).await {
                    (Ok(Ok(res)), _, _) => {
                        let _ = connection_tx.send(Some(res));
                        break 'out;
                    }
                    (Ok(Err(_)), _, remaining) => connections = remaining,
                    (Err(_), _, remaining) => connections = remaining,
                }
            }
            let _ = connection_tx.send(None);
        }
    });

    loop {
        tokio::select! {
            connection = &mut connection_rx => match connection? {
                Some(connection) => return Ok(Some(connection)),
                None => bail!("Failed to connect"),
            },
            cmd = cmd_rx.recv() => {
                let cmd = cmd.ok_or(anyhow!("No more commands"))?;
                if on_cmd(cmd) {
                    return Ok(None);
                }
            }
        }
    }
}

// pub fn hexdump(data: &[u8]) -> String {
//     let mut res = String::new();
//     macro_rules! maybe_display_char {
//         ($f:expr, $byte:expr) => {
//             res.push(if $byte.is_ascii() && !$byte.is_ascii_control() {
//                 $byte as char
//             } else {
//                 '.'
//             })
//         };
//     }
//     let chunks = data.chunks_exact(16);
//     let rem = chunks.remainder();
//     for (i, chunk) in chunks.enumerate() {
//         res += &format!("{:08x}: ", i * 16);
//         for b in chunk {
//             res += &format!("{b:02x} ");
//         }
//         res += " |";
//         for b in chunk {
//             maybe_display_char!(f, *b);
//         }
//         res += "|\n";
//     }

//     if rem.is_empty() {
//         return res;
//     }

//     res += &format!("{:08x}: ", data.len() / 16 * 16);

//     for b in rem {
//         res += &format!("{b:02x} ");
//     }
//     res.push(' ');
//     for _ in rem.len()..16 {
//         res += "   ";
//     }

//     res.push('|');
//     for b in rem {
//         maybe_display_char!(f, *b);
//     }
//     res += "|\n";
//     res
// }
