use std::sync::Arc;

use crate::Event;
use crate::common::{Packet, read_packet, write_packet};
use anyhow::Result;
use fcast_protocol::VersionMessage;
use futures::stream::unfold;
use log::{debug, error, trace, warn};
use tokio::sync::mpsc::Sender;
use tokio::{io::AsyncWriteExt, net::TcpStream, sync::broadcast::Receiver};
use tokio_stream::StreamExt;

pub type SessionId = u64;

pub struct Session {
    stream: TcpStream,
    id: SessionId,
}

impl Session {
    pub fn new(stream: TcpStream, id: SessionId) -> Self {
        Self { stream, id }
    }

    pub async fn run(
        mut self,
        updates_rx: Receiver<Arc<Vec<u8>>>,
        event_tx: &Sender<Event>,
    ) -> Result<()> {
        debug!("id={} Session was started", self.id);

        let (tcp_stream_rx, mut tcp_stream_tx) = self.stream.split();

        let packets_stream = unfold(tcp_stream_rx, |mut tcp_stream| async move {
            match read_packet(&mut tcp_stream).await {
                Ok(p) => Some((p, tcp_stream)),
                Err(err) => {
                    error!("Failed to receive packet: {err}");
                    None
                }
            }
        });

        let updates_stream = unfold(
            updates_rx,
            |mut updates_rx: Receiver<Arc<Vec<u8>>>| async move {
                updates_rx
                    .recv()
                    .await
                    .ok()
                    .map(|update| (update, updates_rx))
            },
        );

        tokio::pin!(packets_stream);
        tokio::pin!(updates_stream);

        write_packet(
            &mut tcp_stream_tx,
            Packet::Version(VersionMessage { version: 2 }),
        )
        .await?;

        loop {
            tokio::select! {
                r = packets_stream.next() => {
                    let Some(packet) = r else {
                        break;
                    };

                    trace!("id={} Got packet: {packet:?}", self.id);

                    match packet {
                        Packet::None => (),
                        Packet::Play(play_message) => {
                            event_tx.send(Event::Play(play_message)).await?
                        }
                        Packet::Pause => event_tx.send(Event::Pause).await?,
                        Packet::Resume => event_tx.send(Event::Resume).await?,
                        Packet::Stop => event_tx.send(Event::Stop).await?,
                        Packet::Seek(seek_message) => {
                            event_tx.send(Event::Seek(seek_message)).await?
                        }
                        Packet::SetVolume(set_volume_message) => {
                            event_tx.send(Event::SetVolume(set_volume_message)).await?;
                        }
                        Packet::SetSpeed(set_speed_message) => {
                            event_tx.send(Event::SetSpeed(set_speed_message)).await?;
                        }
                        Packet::Ping => write_packet(&mut tcp_stream_tx, Packet::Pong).await?,
                        Packet::Pong => trace!("id={} Got pong from sender", self.id),
                        _ => warn!(
                            "id={} Invalid packet from sender packet={packet:?}",
                            self.id
                        ),
                    }
                }
                r = updates_stream.next() => {
                    let Some(update) = r else {
                        break;
                    };

                    tcp_stream_tx.write_all(&update).await?;
                    trace!("id={} Sent update", self.id);
                }
            }
        }

        Ok(())
    }
}
