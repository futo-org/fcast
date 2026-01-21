use file_server::FileServer;
use futures::StreamExt;
use simply_colored::{GREEN, RED, RESET};
use std::{
    collections::{HashMap, VecDeque},
    io::Write,
    net::SocketAddr,
    path::PathBuf,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc::unbounded_channel,
};
use tracing::{debug, error, info, instrument, warn};

use anyhow::{Context, anyhow, bail, ensure};
use clap::{Parser, Subcommand};
use fast::{Step, TestCase};
use fcast_protocol::{
    HEADER_LENGTH, Opcode, PlaybackState, SetVolumeMessage, VersionMessage, v2,
    v3::{self, InitialSenderMessage},
};

#[derive(Subcommand)]
enum Command {
    RunAll,
}

#[derive(Parser)]
struct Cli {
    /// The host address of the receiver
    #[arg(long, short('H'), default_value_t = String::from("127.0.0.1"))]
    host: String,
    /// The port of the receiver
    #[arg(long, short, default_value_t = 46899)]
    port: u16,
    #[arg(long, short, default_value_t = String::from("../fcast-sample-media"))]
    sample_media_dir: String,
    #[command(flatten)]
    verbosity: clap_verbosity_flag::Verbosity<clap_verbosity_flag::OffLevel>,
    #[command(subcommand)]
    command: Command,
}

const BODY_BUF_LENGTH: usize = 1000 * 32 - 1;

fn current_time_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

// TODO: share reader code between sender sdk and rust receiver
async fn read_packet(
    reader: &mut tokio::net::tcp::ReadHalf<'_>,
    body_buf: &mut [u8; BODY_BUF_LENGTH],
) -> anyhow::Result<(Opcode, Option<String>)> {
    let mut header_buf: [u8; HEADER_LENGTH] = [0; HEADER_LENGTH];

    reader.read_exact(&mut header_buf).await?;

    let opcode = Opcode::try_from(header_buf[4])?;
    let body_length =
        u32::from_le_bytes([header_buf[0], header_buf[1], header_buf[2], header_buf[3]]) as usize
            - 1;

    if body_length > body_buf.len() {
        bail!(
            "Message exceeded maximum length: {body_length} > {}",
            body_buf.len()
        );
    }

    let json_body = if body_length > 0 {
        reader.read_exact(body_buf[..body_length].as_mut()).await?;
        Some(String::from_utf8(body_buf[..body_length].to_vec())?)
    } else {
        None
    };

    Ok((opcode, json_body))
}

#[derive(Debug)]
enum Action {
    Nop,
    WaitForPacket,
    WritePacket {
        op: Opcode,
        body: Vec<u8>,
    },
    WriteSimple(Opcode),
    End,
    ServeFile {
        path: &'static str,
        id: u32,
        mime: &'static str,
    },
    SleepMillis(u64),
    WriteSimpleThenWait(Opcode),
}

#[derive(Debug, PartialEq)]
enum InternalState {
    None,
    Sleeping,
    WaitingForVersion,
    WaitingForInitial,
    WaitingForPong,
    WaitingForVolume,
    WaitingForPlaybackUpdate,
}

struct State {
    steps: &'static [Step],
    internal: InternalState,
    current_step: usize,
    expected_volume: Option<(f64, u64)>,
    current_version: u64,
    expected_play_update: Option<v3::PlayMessage>,
    expecting_pause: bool,
    expecting_resume: bool,
    invalid_volume_updates_received: usize,
}

impl State {
    fn new(steps: &'static [Step]) -> Self {
        Self {
            steps,
            internal: InternalState::None,
            current_step: 0,
            expected_volume: None,
            current_version: 0,
            expected_play_update: None,
            expecting_pause: false,
            expecting_resume: false,
            invalid_volume_updates_received: 0,
        }
    }

    #[instrument(skip_all)]
    fn received_packet(
        &mut self,
        (opcode, body): (Opcode, Option<String>),
    ) -> anyhow::Result<Option<Action>> {
        debug!(?opcode, ?body, "Received packet");

        if opcode == Opcode::Ping {
            return Ok(Some(Action::WriteSimpleThenWait(Opcode::Pong)));
        }

        match opcode {
            // TODO: check that it matches what we expect
            Opcode::PlaybackUpdate => {
                let msg = serde_json::from_str::<v3::PlaybackUpdateMessage>(
                    &body
                        .clone()
                        .ok_or(anyhow!("Playback update is missing body"))?,
                )?;

                if msg.state != PlaybackState::Idle && self.expecting_pause {
                    if msg.state == PlaybackState::Paused {
                        self.expecting_pause = false;
                        info!("Paused state correct");
                    } else {
                        bail!("Expected paused state got {:?}", msg.state);
                    }
                } else if msg.state != PlaybackState::Idle && self.expecting_resume {
                    if msg.state == PlaybackState::Playing {
                        self.expecting_resume = false;
                        info!("Playing state correct");
                    } else {
                        bail!("Expected playing state got {:?}", msg.state);
                    }
                }

                // return Ok(Some(Action::WaitForPacket));
            }
            Opcode::VolumeUpdate => {
                let msg = serde_json::from_str::<v2::VolumeUpdateMessage>(
                    &body
                        .clone()
                        .ok_or(anyhow!("Volume update is missing body"))?,
                )?;

                const MAX_INVALID_VOLUME_UPDATES: usize = 3;

                if let Some(expected) = self.expected_volume
                    && expected.1 <= msg.generation_time
                {
                    if let Some(expected_volume) = self.expected_volume {
                        if (msg.volume - expected_volume.0).abs() <= 0.001 {
                            self.expected_volume = None;
                            self.invalid_volume_updates_received = 0;
                            info!("Volume correct");
                        } else {
                            self.invalid_volume_updates_received += 1;
                            if self.invalid_volume_updates_received >= MAX_INVALID_VOLUME_UPDATES {
                                panic!(
                                    "Invalid volume. expected: {:?} got: {:?}",
                                    expected_volume,
                                    (msg.volume, msg.generation_time)
                                );
                            } else {
                                warn!(
                                    "Received invalid volume on retry {}. expected: {:?} got: {}",
                                    self.invalid_volume_updates_received,
                                    expected_volume,
                                    msg.volume
                                );
                                return Ok(None);
                            }
                        }
                    }
                }
            }
            Opcode::PlayUpdate => {
                let msg = serde_json::from_str::<v3::PlayUpdateMessage>(
                    &body.clone().ok_or(anyhow!("Play update is missing body"))?,
                )?;
                if let Some(expected_update) = self.expected_play_update.take() {
                    assert_eq!(msg.play_data.unwrap(), expected_update);
                    info!("Play update correct");
                }
            }
            _ => (),
        }

        match self.internal {
            InternalState::None => (),
            InternalState::Sleeping => return Ok(None),
            InternalState::WaitingForVersion => match opcode {
                Opcode::Version => {
                    serde_json::from_str::<VersionMessage>(
                        &body.ok_or(anyhow!("Version is missing body"))?,
                    )?;
                }
                _ => panic!(),
            },
            InternalState::WaitingForInitial => match opcode {
                Opcode::Initial => {
                    serde_json::from_str::<v3::InitialReceiverMessage>(
                        &body.ok_or(anyhow!("Initial is missing body"))?,
                    )?;
                }
                _ => panic!(),
            },
            InternalState::WaitingForPong => match opcode {
                Opcode::Pong => (),
                _ => panic!(),
            },
            InternalState::WaitingForPlaybackUpdate => match opcode {
                Opcode::PlaybackUpdate => (),
                _ => panic!(),
            },
            _ => (),
        }

        self.internal = InternalState::None;

        Ok(None)
    }

    fn ready(&self) -> bool {
        debug!(
            b = (self.expected_volume.is_none()),
            c = (self.expected_play_update.is_none()),
            d = (!self.expecting_pause),
            e = (!self.expecting_resume),
            sleeping = self.internal != InternalState::Sleeping
        );

        self.expected_volume.is_none()
            && self.expected_play_update.is_none()
            && !self.expecting_pause
            && !self.expecting_resume
            && self.internal != InternalState::Sleeping
    }

    #[instrument(skip_all)]
    fn next_state(
        &mut self,
        file_urls: &HashMap<u32, (String, &'static str)>,
    ) -> anyhow::Result<Option<Action>> {
        if !self.ready() {
            return Ok(Some(Action::WaitForPacket));
        }

        match self.internal {
            InternalState::None => {
                let Some(next) = self.steps.get(self.current_step) else {
                    return Ok(Some(Action::End));
                };
                self.current_step += 1;

                debug!(next_state = ?next, "Have next state");

                match next {
                    Step::Send(send) => match send {
                        fast::Send::Version(version) => {
                            let body = VersionMessage { version: *version };
                            self.current_version = *version;
                            return Ok(Some(Action::WritePacket {
                                op: Opcode::Version,
                                body: serde_json::to_vec(&body)?,
                            }));
                        }
                        fast::Send::Initial => {
                            let body = InitialSenderMessage {
                                display_name: Some("test".to_owned()),
                                app_name: Some("test".to_owned()),
                                app_version: Some("test".to_owned()),
                            };
                            return Ok(Some(Action::WritePacket {
                                op: Opcode::Initial,
                                body: serde_json::to_vec(&body).unwrap(),
                            }));
                        }
                        fast::Send::Ping => {
                            return Ok(Some(Action::WriteSimple(Opcode::Ping)));
                        }
                        fast::Send::SetVolume(volume) => {
                            let body = SetVolumeMessage { volume: *volume };
                            self.expected_volume = Some((*volume, current_time_millis()));
                            return Ok(Some(Action::WritePacket {
                                op: Opcode::SetVolume,
                                body: serde_json::to_vec(&body).unwrap(),
                            }));
                        }
                        fast::Send::Stop => {
                            return Ok(Some(Action::WriteSimple(Opcode::Stop)));
                        }
                        fast::Send::PlayV2 { file_id } => {
                            let (url, mime) = file_urls.get(file_id).unwrap();
                            let body = v2::PlayMessage {
                                container: mime.to_string(),
                                url: Some(url.clone()),
                                content: None,
                                time: None,
                                speed: None,
                                headers: None,
                            };
                            return Ok(Some(Action::WritePacket {
                                op: Opcode::Play,
                                body: serde_json::to_vec(&body).unwrap(),
                            }));
                        }
                        fast::Send::PlayV3 { file_id } => {
                            let (url, mime) = file_urls.get(file_id).unwrap();
                            let body = v3::PlayMessage {
                                container: mime.to_string(),
                                url: Some(url.clone()),
                                content: None,
                                time: None,
                                speed: None,
                                headers: None,
                                volume: None,
                                metadata: None,
                            };
                            self.expected_play_update = Some(body.clone());
                            return Ok(Some(Action::WritePacket {
                                op: Opcode::Play,
                                body: serde_json::to_vec(&body).unwrap(),
                            }));
                        }
                        fast::Send::Pause => {
                            self.expecting_pause = true;
                            return Ok(Some(Action::WriteSimple(Opcode::Pause)));
                        }
                        fast::Send::Resume => {
                            self.expecting_resume = true;
                            return Ok(Some(Action::WriteSimple(Opcode::Resume)));
                        }
                    },
                    Step::Receive(receive) => {
                        self.internal = match receive {
                            fast::Receive::Version => InternalState::WaitingForVersion,
                            fast::Receive::Initial => InternalState::WaitingForInitial,
                            fast::Receive::Pong => InternalState::WaitingForPong,
                            fast::Receive::Volume => InternalState::WaitingForVolume,
                            fast::Receive::PlaybackUpdate => {
                                InternalState::WaitingForPlaybackUpdate
                            }
                        };
                        return Ok(Some(Action::WaitForPacket));
                    }
                    Step::ServeFile { path, id, mime } => {
                        return Ok(Some(Action::ServeFile {
                            path,
                            id: *id,
                            mime,
                        }));
                    }
                    Step::SleepMillis(ms) => {
                        self.internal = InternalState::Sleeping;
                        return Ok(Some(Action::SleepMillis(*ms)));
                    }
                }
            }
            InternalState::Sleeping => Ok(Some(Action::Nop)),
            InternalState::WaitingForVersion
            | InternalState::WaitingForInitial
            | InternalState::WaitingForPong
            | InternalState::WaitingForVolume
            | InternalState::WaitingForPlaybackUpdate => Ok(Some(Action::WaitForPacket)),
        }
    }

    fn sleep_finished(&mut self) {
        self.internal = InternalState::None;
    }

    fn finish(&self) -> anyhow::Result<()> {
        ensure!(self.current_step == self.steps.len());
        ensure!(self.expected_volume.is_none());
        ensure!(self.expected_play_update.is_none());
        ensure!(!self.expecting_pause);
        ensure!(!self.expecting_resume);
        Ok(())
    }
}

async fn write_simple(
    writer: &mut tokio::net::tcp::WriteHalf<'_>,
    op: Opcode,
) -> anyhow::Result<()> {
    debug!("SEND {op:?}");
    let mut header = vec![0u8; HEADER_LENGTH];
    header[..HEADER_LENGTH - 1].copy_from_slice(&1u32.to_le_bytes());
    header[HEADER_LENGTH - 1] = op as u8;
    writer
        .write_all(&header)
        .await
        .context("Failed to write header")?;
    // tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(())
}

async fn run_test(
    receiver: &SocketAddr,
    file_server: &FileServer,
    sample_media_path: &PathBuf,
    test: &TestCase,
) -> anyhow::Result<()> {
    let mut stream = TcpStream::connect(receiver).await.unwrap();
    let local_addr = stream.local_addr().unwrap().ip();
    let mut state = State::new(test.steps);
    let mut file_urls: HashMap<u32, (String, &'static str)> = HashMap::new();
    let (reader, mut writer) = stream.split();
    let (sleep_tx, mut sleep_rx) = unbounded_channel::<()>();
    let mut action_queue = VecDeque::new();
    action_queue.push_back(state.next_state(&file_urls)?);

    let packet_stream = futures::stream::unfold(
        (reader, Box::new([0u8; 1000 * 32 - 1])),
        |(mut reader, mut body_buf)| async move {
            match read_packet(&mut reader, &mut body_buf).await {
                Ok((op, json)) => {
                    debug!("Received packet with opcode: {op:?}, body: {json:?}");
                    Some(((op, json), (reader, body_buf)))
                }
                Err(err) => {
                    error!("Error occurred while reading packet: {err}");
                    None
                }
            }
        },
    );

    tokio::pin!(packet_stream);

    'out: loop {
        debug!("{action_queue:?}");

        while let Some(curr_action) = action_queue.pop_front() {
            let Some(curr_action) = curr_action else {
                break 'out;
            };

            debug!(?curr_action, "Handling action");

            let mut get_next_action = true;
            let mut handle_action_immediately = true;

            match curr_action {
                Action::Nop => {
                    get_next_action = false;
                }
                Action::WaitForPacket => {
                    handle_action_immediately = false;
                    get_next_action = false;
                }
                Action::WriteSimple(op) => {
                    write_simple(&mut writer, op).await?;
                }
                Action::WritePacket { op, body } => {
                    debug!("SEND {op:?} {}", String::from_utf8_lossy(&body));
                    let mut header = vec![0u8; HEADER_LENGTH];
                    let size = body.len() + 1;
                    header[..HEADER_LENGTH - 1].copy_from_slice(&(size as u32).to_le_bytes());
                    header[HEADER_LENGTH - 1] = op as u8;
                    writer
                        .write_all(&header)
                        .await
                        .context("Failed to write header")?;
                    writer
                        .write_all(&body)
                        .await
                        .context("Failed to write body")?;
                    // tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Action::End => break 'out,
                Action::ServeFile { path, id, mime } => {
                    let mut file_path = sample_media_path.clone();
                    file_path.push(path);
                    ensure!(file_path.exists());
                    let file_id = file_server.add_file(file_path, mime);
                    let url = file_server.get_url(&(local_addr.into()), &file_id);
                    let _ = file_urls.insert(id, (url, mime));
                }
                Action::SleepMillis(ms) => {
                    let sleep_tx = sleep_tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                        let _ = sleep_tx.send(());
                    });
                    break;
                }
                Action::WriteSimpleThenWait(op) => {
                    write_simple(&mut writer, op).await?;
                    get_next_action = false;
                    handle_action_immediately = false;
                }
            }

            if get_next_action {
                action_queue.push_back(state.next_state(&file_urls)?);
            }

            if handle_action_immediately {
                continue;
            }
        }

        tokio::select! {
            packet = packet_stream.next() => {
                let packet = packet.ok_or(anyhow::anyhow!("Packet stream ended"))?;
                debug!("RECV {packet:?}");
                if let Some(next_action) = state.received_packet(packet)? {
                    action_queue.push_back(Some(next_action));
                }
                action_queue.push_back(state.next_state(&file_urls)?);
            }
            _ = sleep_rx.recv() => {
                state.sleep_finished();
                action_queue.push_back(state.next_state(&file_urls)?);
            }
        }
    }

    state.finish()
}

async fn run_all_tests(receiver: SocketAddr, sample_media_path: PathBuf) {
    let file_server = FileServer::new(0).await.unwrap();
    let mut stdout = std::io::stdout();

    for (idx, case) in fast::TEST_CASES.iter().enumerate() {
        print!("test {} ...", case.name);
        stdout.flush().unwrap();
        match run_test(&receiver, &file_server, &sample_media_path, case).await {
            Ok(_) => {
                println!("\rtest {} ... {GREEN}OK{RESET}", case.name);
            }
            Err(err) => {
                println!("\rtest {} ... {RED}FAILED{RESET}", case.name);
                println!("Reason: {err:?}");
                return;
            }
        }

        if idx != fast::TEST_CASES.len() - 1 {
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let sample_media_path = PathBuf::from(cli.sample_media_dir);

    tracing_subscriber::fmt()
        .with_max_level(cli.verbosity)
        .init();

    let receiver = SocketAddr::new(cli.host.parse().unwrap(), cli.port);

    match cli.command {
        Command::RunAll => run_all_tests(receiver, sample_media_path).await,
    }
}
