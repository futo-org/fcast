#[cfg(any_protocol)]
mod any_protocol_prelude {
    pub use std::{net::SocketAddr, time::Duration};

    pub use anyhow::{anyhow, bail};
    pub use log::debug;
    pub use tokio::net::TcpStream;
}

#[cfg(any_protocol)]
use any_protocol_prelude::*;

/// The head start each address gets over the one behind it (see
/// [`try_connect_tcp`]). Long enough that a reachable LAN address always
/// answers inside it, short enough that a dead candidate ahead of a live one
/// costs a blink rather than the connect deadline.
#[cfg(any_protocol)]
const CONNECT_STAGGER: Duration = Duration::from_millis(150);

/// # Arguments
///
///    * on_cmd: return true if the connect loop should quit.
#[cfg(any_protocol)]
pub(crate) async fn try_connect_tcp<T>(
    addrs: &[SocketAddr],
    timeout: Duration,
    cmd_rx: &mut tokio::sync::mpsc::UnboundedReceiver<T>,
    on_cmd: impl Fn(T) -> bool,
) -> anyhow::Result<Option<tokio::net::TcpStream>> {
    anyhow::ensure!(!addrs.is_empty());

    debug!("Trying to connect to {addrs:?}...");

    // STAGGERED, not all at once. The caller hands these over in preference
    // order (`device::prefer_routable`: routable before link-local, because
    // every address derived from a link-local socket is unusable off-host,
    // the file server's URL among them), and racing them all left that order
    // deciding nothing: on one LAN both answer in about a millisecond and
    // whichever future happened to finish first won. A head start per
    // candidate lets the preferred address win whenever it is reachable, and
    // still falls through to the next rather than waiting out the whole
    // timeout, which a plain sequential dial would cost against a stale
    // advertised address.
    let mut connections: Vec<_> = addrs
        .iter()
        .enumerate()
        .map(|(rank, addr)| {
            let addr = *addr;
            let head_start = CONNECT_STAGGER * rank as u32;
            Box::pin(tokio::time::timeout(timeout + head_start, async move {
                tokio::time::sleep(head_start).await;
                TcpStream::connect(addr).await
            }))
        })
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

#[cfg(any_protocol)]
#[derive(Debug)]
pub(crate) enum WorkError {
    DidNotConnect(String),
    Anyhow(anyhow::Error),
    Io(std::io::Error),
    SerdeJson(serde_json::Error),
    Disconnected,
    ReceivePacket,
}

#[cfg(any_protocol)]
impl From<anyhow::Error> for WorkError {
    fn from(value: anyhow::Error) -> Self {
        Self::Anyhow(value)
    }
}

#[cfg(any_protocol)]
impl From<std::io::Error> for WorkError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[cfg(any_protocol)]
impl From<serde_json::Error> for WorkError {
    fn from(value: serde_json::Error) -> Self {
        Self::SerdeJson(value)
    }
}

#[cfg(any_protocol)]
impl std::error::Error for WorkError {}

#[cfg(any_protocol)]
impl std::fmt::Display for WorkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkError::DidNotConnect(err) => write!(f, "Did not connect: {err:?}"),
            WorkError::Anyhow(err) => write!(f, "{err:?}"),
            WorkError::Io(err) => write!(f, "{err:?}"),
            WorkError::SerdeJson(err) => write!(f, "{err:?}"),
            WorkError::Disconnected => write!(f, "Disconnected"),
            WorkError::ReceivePacket => write!(f, "Received invalid packet"),
        }
    }
}

#[cfg(any_protocol)]
#[macro_export]
macro_rules! connection_loop {
    ($reconnect_interval_millis:expr, on_work = $on_work: block, on_reconnect_started = $on_reconnect_started:block) => {{
        let reconnect_duration = Duration::from_millis($reconnect_interval_millis);
        loop {
            match ($on_work) {
                Ok(_) => break,
                Err(err) => {
                    error!("Inner work error: {err}");
                    if $reconnect_interval_millis == 0 {
                        break;
                    } else {
                        if !matches!(err, $crate::utils::WorkError::DidNotConnect(_)) {
                            $on_reconnect_started;
                        }

                        tokio::time::sleep(reconnect_duration).await;
                    }
                }
            }
        }
    }};
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

#[cfg(all(test, any_protocol))]
mod tests {
    use super::*;

    /// The preferred address wins when both answer.
    ///
    /// List order alone never decided this: `select_all` polls in order but
    /// the futures run concurrently, so on a LAN where both answer in about a
    /// millisecond the winner was whichever finished first. Both listeners
    /// here are already accepting, so the head start is the only thing that
    /// can decide it.
    #[tokio::test]
    async fn the_preferred_address_wins_when_both_answer() {
        let preferred = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fallback = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let preferred_addr = preferred.local_addr().unwrap();
        let fallback_addr = fallback.local_addr().unwrap();
        for listener in [preferred, fallback] {
            tokio::spawn(async move {
                let _accepted = listener.accept().await;
                std::future::pending::<()>().await;
            });
        }

        let (_tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let stream = try_connect_tcp(
            &[preferred_addr, fallback_addr],
            Duration::from_secs(5),
            &mut rx,
            |()| false,
        )
        .await
        .expect("the connect must not fail")
        .expect("a stream, not a caller quit");
        assert_eq!(
            stream.peer_addr().unwrap(),
            preferred_addr,
            "the first address must be the one dialed when both answer"
        );
    }

    /// And the fallback still happens: a dead candidate ahead of a live one
    /// costs its head start, not the connect deadline.
    #[tokio::test]
    async fn a_dead_first_address_falls_through() {
        let reachable = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let reachable_addr = reachable.local_addr().unwrap();
        // Bound, its port learned, then dropped, so connecting is refused.
        let dead_addr = {
            let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            dead.local_addr().unwrap()
        };
        tokio::spawn(async move {
            let _accepted = reachable.accept().await;
            std::future::pending::<()>().await;
        });

        let (_tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let started = std::time::Instant::now();
        let stream = try_connect_tcp(
            &[dead_addr, reachable_addr],
            Duration::from_secs(5),
            &mut rx,
            |()| false,
        )
        .await
        .expect("the connect must not fail")
        .expect("a stream, not a caller quit");
        assert_eq!(stream.peer_addr().unwrap(), reachable_addr);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the fallback waited out the timeout instead of the head start"
        );
    }
}
