use std::sync::Arc;

use crate::Event;
use crate::common::{Packet, read_packet, write_packet};
use anyhow::Result;
use fcast_protocol::{Opcode, VersionMessage};
use futures::stream::unfold;
use tokio::sync::mpsc::Sender;
use tokio::{io::AsyncWriteExt, net::TcpStream, sync::broadcast::Receiver};
use tokio_stream::StreamExt;
use tracing::{debug, error, trace, warn};

pub type SessionId = u64;

#[derive(Debug)]
enum DriverEvent<'a> {
    Tick,
    Packet { opcode: Opcode, body: &'a str },
}

#[derive(Debug)]
enum Action {
    None,
    Ping,
    Pong,
    EndSession,
}

#[derive(Debug)]
enum SessionVersion {
    // V1,
    V2,
    V3,
}

#[derive(Debug)]
enum StateVariant {
    WaitingForVersion,
    Active { version: SessionVersion },
}

#[derive(Debug)]
struct State {
    time: u32,
    last_packet_received: u32,
    waiting_for_pong: bool,
    variant: StateVariant,
}

impl State {
    pub fn new() -> Self {
        Self {
            time: 0,
            last_packet_received: 0,
            waiting_for_pong: false,
            variant: StateVariant::WaitingForVersion,
        }
    }

    // Play	1	Client message to play a video, body is PlayMessage
    // Pause	2	Client message to pause a video, no body
    // Resume	3	Client message to resume a video, no body
    // Stop	4	Client message to stop a video, no body
    // Seek	5	Client message to seek, body is SeekMessage
    // PlaybackUpdate	6	Receiver message to notify an updated playback state, body is PlaybackUpdateMessage
    // VolumeUpdate	7	Receiver message to notify when the volume has changed, body is VolumeUpdateMessage
    // SetVolume	8	Client message to change volume, body is SetVolumeMessage
    fn handle_packet_uninit(&mut self, opcode: Opcode, body: &str) -> Result<Action> {
        Ok(match opcode {
            Opcode::None => Action::None,
            Opcode::Play => todo!(),
            Opcode::Pause => todo!(),
            Opcode::Resume => todo!(),
            Opcode::Stop => todo!(),
            Opcode::Seek => todo!(),
            Opcode::PlaybackUpdate => todo!(),
            Opcode::VolumeUpdate => todo!(),
            Opcode::SetVolume => todo!(),
            Opcode::PlaybackError => todo!(),
            Opcode::SetSpeed => todo!(),
            Opcode::Version => todo!(),
            Opcode::Ping => Action::Pong,
            Opcode::Pong => todo!(),
            Opcode::Initial => todo!(),
            Opcode::PlayUpdate => todo!(),
            Opcode::SetPlaylistItem => todo!(),
            Opcode::SubscribeEvent => todo!(),
            Opcode::UnsubscribeEvent => todo!(),
            Opcode::Event => todo!(),
        })
    }

    fn handle_packet_v2(&mut self, opcode: Opcode, body: &str) -> Result<Action> {
        todo!();
    }

    fn handle_packet_v3(&mut self, opcode: Opcode, body: &str) -> Result<Action> {
        todo!();
    }

    pub fn advance(&mut self, event: DriverEvent) -> Result<Action> {
        Ok(match event {
            DriverEvent::Tick => {
                self.time += 1;
                let diff = self.time - self.last_packet_received;
                if diff > 3 {
                    if self.waiting_for_pong && diff >= 6 {
                        Action::EndSession
                    } else {
                        self.waiting_for_pong = true;
                        Action::Ping
                    }
                } else {
                    Action::None
                }
            }
            DriverEvent::Packet { opcode, body } => {
                self.last_packet_received = self.time;
                self.waiting_for_pong = false;

                match &self.variant {
                    StateVariant::WaitingForVersion => return self.handle_packet_uninit(opcode, body),
                    StateVariant::Active { version } => {
                        match version {
                            SessionVersion::V2 => return self.handle_packet_v2(opcode, body),
                            SessionVersion::V3 => return self.handle_packet_v3(opcode, body),
                        }
                    }
                }
            }
        })
    }
}

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

pub struct SessionDriver {}

impl SessionDriver {
    pub fn new() -> Self {
        Self {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
}
