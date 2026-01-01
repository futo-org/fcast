use std::{sync::Arc, time::Duration};

use crate::{
    Event,
    common::{HEADER_BUFFER_SIZE, Header, Packet, read_packet, write_packet},
};
use bitflags::bitflags;
use fcast_protocol::{Opcode, SeekMessage, SetSpeedMessage, VersionMessage, v3};
use futures::stream::unfold;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        TcpStream,
        tcp::{ReadHalf, WriteHalf},
    },
    sync::{broadcast::Receiver, mpsc::Sender},
};
use tokio_stream::StreamExt;
use tracing::{debug, error, trace, warn};

pub type SessionId = u64;

const TICKS_BEFORE_PING: u32 = 3;

#[derive(Debug, thiserror::Error, PartialEq)]
enum StateError {
    #[error("invalid json")]
    InvalidJson,
    #[error("illegal version: {0}")]
    IllegalVersion(u64),
    #[error("illegal opcode: {0:?}")]
    IllegalOpcode(Opcode),
    #[error("missing body")]
    MissingBody,
}

#[derive(Debug)]
enum DriverEvent<'a> {
    Tick,
    Packet {
        opcode: Opcode,
        body: Option<&'a str>,
    },
}

#[derive(Debug, PartialEq)]
pub enum Operation {
    Pause,
    Resume,
    Stop,
    Play(v3::PlayMessage),
    Seek(SeekMessage),
    SetSpeed(SetSpeedMessage),
    SetPlaylistItem(v3::SetPlaylistItemMessage),
}

#[derive(Debug, PartialEq)]
enum Action {
    None,
    Ping,
    Pong,
    EndSession,
    Op(Operation),
}

#[derive(Debug, PartialEq)]
enum SessionVersion {
    V1,
    V2,
    V3,
}

#[derive(Debug, PartialEq)]
enum StateVariant {
    WaitingForVersion,
    Active { version: SessionVersion },
}

bitflags! {
    #[derive(Debug)]
    struct MediaItemEventFlags: u8 {
        const Start = 1;
        const End = 1 << 1;
        const Changed = 1 << 2;
    }
}

bitflags! {
    #[derive(Debug)]
    struct KeyEventFlags: u8 {
        const ArrowLeft = 1;
        const ArrowRight = 1 << 1;
        const ArrowUp = 1 << 2;
        const ArrowDown = 1 << 3;
        const Enter = 1 << 4;
    }
}

impl KeyEventFlags {
    pub fn from_strings(names: &[String]) -> Self {
        let mut flags = Self::empty();
        for name in names {
            match name.as_str() {
                "ArrowLeft" => flags.insert(Self::ArrowLeft),
                "ArrowRight" => flags.insert(Self::ArrowRight),
                "ArrowUp" => flags.insert(Self::ArrowUp),
                "ArrowDown" => flags.insert(Self::ArrowDown),
                "Enter" => flags.insert(Self::Enter),
                _ => (),
            }
        }
        flags
    }
}

macro_rules! err_body {
    ($res:expr) => {
        $res.ok_or(StateError::MissingBody)?
    };
}

macro_rules! option_err_body {
    ($res:expr) => {
        $res.ok_or(StateError::MissingBody)
            .map_err(|err| Some(err))
            .ok()?
    };
}

macro_rules! err_json {
    ($t:ty, $body:expr) => {
        serde_json::from_str::<$t>($body).map_err(|_| StateError::InvalidJson)?
    };
}

macro_rules! option_err_json {
    ($t:ty, $body:expr) => {
        serde_json::from_str::<$t>($body)
            .map_err(|_| StateError::InvalidJson)
            .map_err(|err| Some(err))
            .ok()?
    };
}

#[derive(Debug)]
struct State {
    time: u32,
    last_packet_received: u32,
    waiting_for_pong: bool,
    variant: StateVariant,
    media_item_events: MediaItemEventFlags,
    key_name_events_down: KeyEventFlags,
    key_name_events_up: KeyEventFlags,
}

impl State {
    pub fn new() -> Self {
        Self {
            time: 0,
            last_packet_received: 0,
            waiting_for_pong: false,
            variant: StateVariant::WaitingForVersion,
            media_item_events: MediaItemEventFlags::empty(),
            key_name_events_down: KeyEventFlags::empty(),
            key_name_events_up: KeyEventFlags::empty(),
        }
    }

    fn handle_packet_uninit(
        &mut self,
        opcode: Opcode,
        body: Option<&str>,
    ) -> Result<Action, StateError> {
        Ok(match opcode {
            Opcode::None
            | Opcode::Play
            | Opcode::Pause
            | Opcode::Resume
            | Opcode::Stop
            | Opcode::Seek
            | Opcode::SetVolume => {
                self.variant = StateVariant::Active {
                    version: SessionVersion::V1,
                };
                return self.handle_packet_v1(opcode, body);
            }
            Opcode::Version => {
                let msg = err_json!(VersionMessage, err_body!(body));
                let version = match msg.version {
                    1 => SessionVersion::V1,
                    2 => SessionVersion::V2,
                    3 => SessionVersion::V3,
                    _ => return Err(StateError::IllegalVersion(msg.version)),
                };
                self.variant = StateVariant::Active { version };
                // TODO: send InitialReceiverMessage on v3
                Action::None
            }
            // TODO: technically v2 doesn't need to accept VersionMessage before starting the session
            _ => return Err(StateError::IllegalOpcode(opcode)),
        })
    }

    /// Handle those packets that are common for v{1, 2, 3}
    // TODO: should it return option or error variant like Unsupported?
    fn handle_packet_common(
        &mut self,
        opcode: Opcode,
        body: Option<&str>,
    ) -> Option<Result<Action, StateError>> {
        Some(Ok(match opcode {
            Opcode::None => todo!(),
            Opcode::Play => {
                let msg = option_err_json!(v3::PlayMessage, option_err_body!(body));
                Action::Op(Operation::Play(msg))
            }
            Opcode::Pause => Action::Op(Operation::Pause),
            Opcode::Resume => Action::Op(Operation::Resume),
            Opcode::Stop => Action::Op(Operation::Stop),
            Opcode::Seek => {
                let msg = option_err_json!(SeekMessage, option_err_body!(body));
                Action::Op(Operation::Seek(msg))
            }
            Opcode::SetVolume => todo!(),
            // Ignore
            Opcode::PlaybackUpdate
            | Opcode::VolumeUpdate
            | Opcode::PlayUpdate
            | Opcode::PlaybackError => Action::None,
            _ => return None,
        }))
    }

    fn handle_packet_v1(
        &mut self,
        opcode: Opcode,
        body: Option<&str>,
    ) -> Result<Action, StateError> {
        if let Some(res) = self.handle_packet_common(opcode, body) {
            return res;
        };

        Err(StateError::IllegalOpcode(opcode))
    }

    fn handle_packet_v2(
        &mut self,
        opcode: Opcode,
        body: Option<&str>,
    ) -> Result<Action, StateError> {
        if let Some(res) = self.handle_packet_common(opcode, body) {
            return res;
        };

        Ok(match opcode {
            Opcode::Ping => Action::Pong,
            Opcode::Pong => Action::None,
            Opcode::SetSpeed => {
                let msg = err_json!(SetSpeedMessage, err_body!(body));
                Action::Op(Operation::SetSpeed(msg))
            }
            _ => return Err(StateError::IllegalOpcode(opcode)),
        })
    }

    fn handle_packet_v3(
        &mut self,
        opcode: Opcode,
        body: Option<&str>,
    ) -> Result<Action, StateError> {
        if let Some(res) = self.handle_packet_common(opcode, body) {
            return res;
        };

        let v2_res = self.handle_packet_v2(opcode, body);
        if !matches!(v2_res, Err(StateError::IllegalOpcode(_))) {
            return v2_res;
        }

        Ok(match opcode {
            Opcode::Initial => {
                let _msg = err_json!(v3::InitialSenderMessage, err_body!(body));
                Action::None
            }
            Opcode::SetPlaylistItem => {
                let msg = err_json!(v3::SetPlaylistItemMessage, err_body!(body));
                Action::Op(Operation::SetPlaylistItem(msg))
            }
            Opcode::SubscribeEvent => {
                let msg = err_json!(v3::SubscribeEventMessage, err_body!(body));
                match msg.event {
                    v3::EventSubscribeObject::MediaItemStart => {
                        self.media_item_events.insert(MediaItemEventFlags::Start)
                    }
                    v3::EventSubscribeObject::MediaItemEnd => {
                        self.media_item_events.insert(MediaItemEventFlags::End)
                    }
                    v3::EventSubscribeObject::MediaItemChanged => {
                        self.media_item_events.insert(MediaItemEventFlags::Changed)
                    }
                    v3::EventSubscribeObject::KeyDown { keys } => self
                        .key_name_events_down
                        .insert(KeyEventFlags::from_strings(&keys)),
                    v3::EventSubscribeObject::KeyUp { keys } => self
                        .key_name_events_up
                        .insert(KeyEventFlags::from_strings(&keys)),
                }
                Action::None
            }
            Opcode::UnsubscribeEvent => {
                let msg = err_json!(v3::UnsubscribeEventMessage, err_body!(body));
                match msg.event {
                    v3::EventSubscribeObject::MediaItemStart => {
                        self.media_item_events.remove(MediaItemEventFlags::Start)
                    }
                    v3::EventSubscribeObject::MediaItemEnd => {
                        self.media_item_events.remove(MediaItemEventFlags::End)
                    }
                    v3::EventSubscribeObject::MediaItemChanged => {
                        self.media_item_events.remove(MediaItemEventFlags::Changed)
                    }
                    v3::EventSubscribeObject::KeyDown { keys } => self
                        .key_name_events_down
                        .remove(KeyEventFlags::from_strings(&keys)),
                    v3::EventSubscribeObject::KeyUp { keys } => self
                        .key_name_events_up
                        .remove(KeyEventFlags::from_strings(&keys)),
                }
                Action::None
            }
            _ => return Err(StateError::IllegalOpcode(opcode)),
        })
    }

    pub fn advance(&mut self, event: DriverEvent) -> Result<Action, StateError> {
        Ok(match event {
            DriverEvent::Tick => {
                self.time += 1;
                let diff = self.time - self.last_packet_received;
                if diff > TICKS_BEFORE_PING {
                    if self.waiting_for_pong && diff > TICKS_BEFORE_PING * 2 {
                        Action::EndSession
                    } else if !self.waiting_for_pong {
                        self.waiting_for_pong = true;
                        Action::Ping
                    } else {
                        Action::None
                    }
                } else {
                    Action::None
                }
            }
            DriverEvent::Packet { opcode, body } => {
                self.last_packet_received = self.time;
                self.waiting_for_pong = false;

                match &self.variant {
                    StateVariant::WaitingForVersion => {
                        return self.handle_packet_uninit(opcode, body);
                    }
                    StateVariant::Active { version } => match version {
                        SessionVersion::V1 => return self.handle_packet_v1(opcode, body),
                        SessionVersion::V2 => return self.handle_packet_v2(opcode, body),
                        SessionVersion::V3 => return self.handle_packet_v3(opcode, body),
                    },
                }
            }
        })
    }
}

// pub struct Session {
//     stream: TcpStream,
//     id: SessionId,
// }

// impl Session {
//     pub fn new(stream: TcpStream, id: SessionId) -> Self {
//         todo!()
//         // Self { stream, id }
//     }

//     pub async fn run(
//         mut self,
//         updates_rx: Receiver<Arc<Vec<u8>>>,
//         event_tx: &Sender<Event>,
//     ) -> anyhow::Result<()> {
// debug!("id={} Session was started", self.id);

// let (tcp_stream_rx, mut tcp_stream_tx) = self.stream.split();

// let packets_stream = unfold(tcp_stream_rx, |mut tcp_stream| async move {
//     match read_packet(&mut tcp_stream).await {
//         Ok(p) => Some((p, tcp_stream)),
//         Err(err) => {
//             error!("Failed to receive packet: {err}");
//             None
//         }
//     }
// });

// let updates_stream = unfold(
//     updates_rx,
//     |mut updates_rx: Receiver<Arc<Vec<u8>>>| async move {
//         updates_rx
//             .recv()
//             .await
//             .ok()
//             .map(|update| (update, updates_rx))
//     },
// );

// tokio::pin!(packets_stream);
// tokio::pin!(updates_stream);

// write_packet(
//     &mut tcp_stream_tx,
//     Packet::Version(VersionMessage { version: 2 }),
// )
// .await?;

// loop {
//     tokio::select! {
//         r = packets_stream.next() => {
//             let Some(packet) = r else {
//                 break;
//             };

//             trace!("id={} Got packet: {packet:?}", self.id);

//             match packet {
//                 Packet::None => (),
//                 Packet::Play(play_message) => {
//                     event_tx.send(Event::Play(play_message)).await?
//                 }
//                 Packet::Pause => event_tx.send(Event::Pause).await?,
//                 Packet::Resume => event_tx.send(Event::Resume).await?,
//                 Packet::Stop => event_tx.send(Event::Stop).await?,
//                 Packet::Seek(seek_message) => {
//                     event_tx.send(Event::Seek(seek_message)).await?
//                 }
//                 Packet::SetVolume(set_volume_message) => {
//                     event_tx.send(Event::SetVolume(set_volume_message)).await?;
//                 }
//                 Packet::SetSpeed(set_speed_message) => {
//                     event_tx.send(Event::SetSpeed(set_speed_message)).await?;
//                 }
//                 Packet::Ping => write_packet(&mut tcp_stream_tx, Packet::Pong).await?,
//                 Packet::Pong => trace!("id={} Got pong from sender", self.id),
//                 _ => warn!(
//                     "id={} Invalid packet from sender packet={packet:?}",
//                     self.id
//                 ),
//             }
//         }
//         r = updates_stream.next() => {
//             let Some(update) = r else {
//                 break;
//             };

//             tcp_stream_tx.write_all(&update).await?;
//             trace!("id={} Sent update", self.id);
//         }
//         _ = tick_interval.tick() => {
//         }
//     }
// }

//         Ok(())
//     }
// }

pub struct SessionDriver {
    stream: TcpStream,
    id: SessionId,
    state: State,
}

impl SessionDriver {
    pub fn new(stream: TcpStream, id: SessionId) -> Self {
        Self {
            stream,
            id,
            state: State::new(),
        }
    }

    /// Returns true if the session should end.
    async fn handle_state_result(
        id: SessionId,
        tcp_stream_tx: &mut WriteHalf<'_>,
        event_tx: &Sender<Event>,
        res: Result<Action, StateError>,
    ) -> anyhow::Result<bool> {
        match res {
            Ok(action) => match action {
                Action::None => (),
                Action::Ping => write_packet(tcp_stream_tx, Packet::Ping).await?,
                Action::Pong => write_packet(tcp_stream_tx, Packet::Pong).await?,
                Action::EndSession => return Ok(true),
                Action::Op(operation) => {
                    event_tx
                        .send(Event::Op {
                            session_id: id,
                            op: operation,
                        })
                        .await?;
                }
            },
            Err(err) => {
                error!(?err, "Error occured when advancing state");
                return Ok(true);
            }
        }

        Ok(false)
    }

    async fn read_packet(stream: &mut ReadHalf<'_>) -> anyhow::Result<(Opcode, Option<String>)> {
        let mut header_buf: [u8; HEADER_BUFFER_SIZE] = [0; HEADER_BUFFER_SIZE];

        stream.read_exact(&mut header_buf).await?;

        let header = Header::decode(header_buf);

        let mut body_string = None;

        if header.size > 0 {
            let mut body_buf = vec![0; header.size as usize];
            stream.read_exact(&mut body_buf).await?;
            body_string = Some(String::from_utf8(body_buf)?);
        }

        Ok((header.opcode, body_string))
    }

    // TODO: instrument this in caller with the id etc.
    pub async fn run(
        mut self,
        // TODO: this should contain events that are subscribable
        updates_rx: Receiver<Arc<Vec<u8>>>,
        event_tx: &Sender<Event>,
    ) -> anyhow::Result<()> {
        debug!("id={} Session was started", self.id);

        let (tcp_stream_rx, mut tcp_stream_tx) = self.stream.split();

        let packets_stream = unfold(tcp_stream_rx, |mut tcp_stream| async move {
            match Self::read_packet(&mut tcp_stream).await {
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

        let mut tick_interval = tokio::time::interval(Duration::from_secs(1));

        write_packet(
            &mut tcp_stream_tx,
            Packet::Version(VersionMessage { version: 3 }),
        )
        .await?;

        loop {
            tokio::select! {
                r = packets_stream.next() => {
                    let Some(packet) = r else {
                        break;
                    };

                    trace!("id={} Got packet: {packet:?}", self.id);

                    let opcode = packet.0;
                    let body = packet.1.as_ref();
                    let res = self.state.advance(DriverEvent::Packet { opcode, body: body.map(|b| b.as_str()) });
                    if Self::handle_state_result(self.id, &mut tcp_stream_tx, &event_tx, res).await? {
                        break;
                    }

                    // match packet {
                    //     Packet::None => (),
                    //     Packet::Play(play_message) => {
                    //         event_tx.send(Event::Play(play_message)).await?
                    //     }
                    //     Packet::Pause => event_tx.send(Event::Pause).await?,
                    //     Packet::Resume => event_tx.send(Event::Resume).await?,
                    //     Packet::Stop => event_tx.send(Event::Stop).await?,
                    //     Packet::Seek(seek_message) => {
                    //         event_tx.send(Event::Seek(seek_message)).await?
                    //     }
                    //     Packet::SetVolume(set_volume_message) => {
                    //         event_tx.send(Event::SetVolume(set_volume_message)).await?;
                    //     }
                    //     Packet::SetSpeed(set_speed_message) => {
                    //         event_tx.send(Event::SetSpeed(set_speed_message)).await?;
                    //     }
                    //     Packet::Ping => write_packet(&mut tcp_stream_tx, Packet::Pong).await?,
                    //     Packet::Pong => debug!("Got pong from sender"),
                    //     _ => warn!(
                    //         "id={} Invalid packet from sender packet={packet:?}",
                    //         self.id
                    //     ),
                    // }
                }
                r = updates_stream.next() => {
                    let Some(update) = r else {
                        break;
                    };

                    tcp_stream_tx.write_all(&update).await?;
                    debug!("Sent update");
                }
                _ = tick_interval.tick() => {
                    let res = self.state.advance(DriverEvent::Tick);
                    if Self::handle_state_result(self.id, &mut tcp_stream_tx, &event_tx, res).await? {
                        break;
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_advancements(state: &mut State, events: Vec<(DriverEvent, Result<Action, StateError>)>) {
        for (event, res) in events.into_iter() {
            assert_eq!(state.advance(event), res);
        }
    }

    #[test]
    fn timeout() {
        let mut state = State::new();
        let mut events = Vec::new();
        for _ in 0..TICKS_BEFORE_PING {
            events.push((DriverEvent::Tick, Ok(Action::None)));
        }
        events.push((DriverEvent::Tick, Ok(Action::Ping)));
        for _ in 0..TICKS_BEFORE_PING - 1 {
            events.push((DriverEvent::Tick, Ok(Action::None)));
        }
        events.push((DriverEvent::Tick, Ok(Action::EndSession)));
        run_advancements(&mut state, events);
    }

    #[test]
    fn uninit_to_active() {
        let v2_json = serde_json::to_string(&VersionMessage { version: 2 }).unwrap();
        let v3_json = serde_json::to_string(&VersionMessage { version: 3 }).unwrap();
        let sessions = [
            (
                Opcode::Resume,
                None,
                Action::Op(Operation::Resume),
                SessionVersion::V1,
            ),
            (
                Opcode::Version,
                Some(v2_json.as_str()),
                Action::None,
                SessionVersion::V2,
            ),
            (
                Opcode::Version,
                Some(v3_json.as_str()),
                Action::None,
                SessionVersion::V3,
            ),
        ];

        for (opcode, body, res, version) in sessions {
            let mut state = State::new();
            run_advancements(
                &mut state,
                vec![(DriverEvent::Packet { opcode, body }, Ok(res))],
            );
            assert_eq!(state.variant, StateVariant::Active { version: version });
        }
    }

    #[test]
    fn invalid_json() {
        let mut state = State::new();
        run_advancements(
            &mut state,
            vec![(
                DriverEvent::Packet {
                    opcode: Opcode::Version,
                    body: Some("{"),
                },
                Err(StateError::InvalidJson),
            )],
        );
    }

    #[test]
    fn illegal_opcode() {
        let v2_json = serde_json::to_string(&VersionMessage { version: 2 }).unwrap();
        let mut state = State::new();
        run_advancements(
            &mut state,
            vec![
                (
                    DriverEvent::Packet {
                        opcode: Opcode::Version,
                        body: Some(v2_json.as_str()),
                    },
                    Ok(Action::None),
                ),
                (
                    DriverEvent::Packet {
                        opcode: Opcode::Initial,
                        body: None,
                    },
                    Err(StateError::IllegalOpcode(Opcode::Initial)),
                ),
            ],
        );
    }
}
