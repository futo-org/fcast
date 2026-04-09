use std::{collections::HashSet, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use fast::{PlaylistItem, Receive, Send, Step, engine::run_case};
use fcast_protocol::{
    Opcode, PacketReader, PlaybackErrorMessage, PlaybackState, ReadResult, SeekMessage,
    SetSpeedMessage, SetVolumeMessage, VersionMessage, companion,
    v2::{PlaybackUpdateMessage as V2PlaybackUpdateMessage, VolumeUpdateMessage},
    v3::{
        AVCapabilities, EventMessage, EventObject, EventSubscribeObject, EventType,
        InitialReceiverMessage, LivestreamCapabilities, MediaItem, PlayMessage, PlayUpdateMessage,
        PlaybackUpdateMessage, PlaylistContent, ReceiverCapabilities, SetPlaylistItemMessage,
        SubscribeEventMessage, UnsubscribeEventMessage,
    },
    v4,
};
use file_server::FileServer;
use serde::Serialize;
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_rustls::TlsAcceptor;

#[derive(Clone, Copy, Default)]
struct MockOptions {
    chatty: bool,
    stale_state: bool,
    dribble: bool,
    prelude_burst: bool,
    error_after_handshake: bool,
    close_on_initial: bool,
    bad_json_initial: bool,
    unknown_opcode_initial: bool,
    oversized_initial: bool,
    playback_error_on_play: bool,
    wrong_play_echo: bool,
    wrong_event_item: bool,
    v2_playback_format: bool,
    rich_initial: bool,
}

#[derive(Default)]
struct MockState {
    subs: HashSet<EventSubscribeObject>,
    playlist: Vec<MediaItem>,
}

struct Mock {
    stream: TcpStream,
    opts: MockOptions,
    state: MockState,
}

fn millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn json<T: Serialize>(v: &T) -> Option<Vec<u8>> {
    Some(serde_json::to_vec(v).unwrap())
}

fn frame(opcode: u8, body: Option<&[u8]>) -> Vec<u8> {
    let body_len = body.map_or(0, <[u8]>::len);
    let mut out = Vec::with_capacity(5 + body_len);
    out.extend_from_slice(&((body_len + 1) as u32).to_le_bytes());
    out.push(opcode);
    if let Some(b) = body {
        out.extend_from_slice(b);
    }
    out
}

fn playback_update(state: PlaybackState) -> Option<Vec<u8>> {
    json(&PlaybackUpdateMessage {
        generation_time: millis(),
        state,
        time: Some(1.0),
        duration: Some(100.0),
        speed: Some(1.0),
        item_index: None,
    })
}

fn playback_update_v2(state: PlaybackState) -> Option<Vec<u8>> {
    json(&V2PlaybackUpdateMessage {
        generation_time: millis(),
        time: 1.0,
        duration: 100.0,
        speed: 1.0,
        state,
    })
}

async fn spawn_mock(opts: MockOptions) -> SocketAddr {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            Mock {
                stream,
                opts,
                state: MockState::default(),
            }
            .run()
            .await;
        }
    });
    addr
}

impl Mock {
    async fn send_bytes(&mut self, bytes: &[u8]) {
        if self.opts.dribble {
            for b in bytes {
                if self.stream.write_all(&[*b]).await.is_err() {
                    return;
                }
                let _ = self.stream.flush().await;
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        } else {
            let _ = self.stream.write_all(bytes).await;
        }
    }

    async fn send(&mut self, opcode: Opcode, body: Option<Vec<u8>>) {
        let bytes = frame(opcode as u8, body.as_deref());
        self.send_bytes(&bytes).await;
    }

    async fn send_state(&mut self, state: PlaybackState) {
        let body = if self.opts.v2_playback_format {
            playback_update_v2(state)
        } else {
            playback_update(state)
        };
        self.send(Opcode::PlaybackUpdate, body).await;
    }

    async fn emit_item_events(&mut self, item: &MediaItem) {
        for (sub, variant) in [
            (
                EventSubscribeObject::MediaItemStart,
                EventType::MediaItemStart,
            ),
            (
                EventSubscribeObject::MediaItemChanged,
                EventType::MediaItemChange,
            ),
            (EventSubscribeObject::MediaItemEnd, EventType::MediaItemEnd),
        ] {
            if self.state.subs.contains(&sub) {
                let body = json(&EventMessage {
                    generation_time: millis(),
                    event: EventObject::MediaItem {
                        variant,
                        item: item.clone(),
                    },
                });
                self.send(Opcode::Event, body).await;
            }
        }
    }

    async fn run(mut self) {
        let mut intro = frame(
            Opcode::Version as u8,
            json(&VersionMessage { version: 3 }).as_deref(),
        );
        if self.opts.prelude_burst {
            for _ in 0..5 {
                intro.extend_from_slice(&frame(
                    Opcode::PlaybackUpdate as u8,
                    playback_update(PlaybackState::Playing).as_deref(),
                ));
            }
        }
        let _ = self.stream.write_all(&intro).await;

        let mut reader = PacketReader::new(512 * 1024, 8192);
        let mut buf = [0u8; 8192];
        let mut chatter = tokio::time::interval(Duration::from_millis(75));
        let mut ticks = 0u64;

        loop {
            tokio::select! {
                read = self.stream.read(&mut buf) => {
                    let Ok(n) = read else { break };
                    if n == 0 || reader.push_data(&buf[..n]).is_err() {
                        break;
                    }

                    let mut packets: Vec<(u8, Option<Vec<u8>>)> = Vec::new();
                    loop {
                        match reader.get_packet() {
                            ReadResult::NeedData | ReadResult::PacketTooLarge(_) => break,
                            ReadResult::Read(pkt) => {
                                if let Some((&op, rest)) = pkt.split_first() {
                                    packets.push((op, (!rest.is_empty()).then(|| rest.to_vec())));
                                }
                            }
                        }
                    }

                    for (op, body) in packets {
                        let Ok(opcode) = Opcode::try_from(op) else { continue };
                        if !self.handle(opcode, body).await {
                            return;
                        }
                    }
                }
                _ = chatter.tick(), if self.opts.chatty => {
                    ticks += 1;
                    self.send(Opcode::PlaybackUpdate, playback_update(PlaybackState::Playing)).await;
                    if ticks.is_multiple_of(4) {
                        self.send(Opcode::Ping, None).await;
                    }
                }
            }
        }
    }

    async fn handle(&mut self, opcode: Opcode, body: Option<Vec<u8>>) -> bool {
        match opcode {
            Opcode::Ping => self.send(Opcode::Pong, None).await,
            Opcode::Initial => {
                if self.opts.close_on_initial {
                    return false;
                }
                if self.opts.bad_json_initial {
                    self.send_bytes(&frame(Opcode::Initial as u8, Some(b"{ not json")))
                        .await;
                    return true;
                }
                if self.opts.unknown_opcode_initial {
                    self.send_bytes(&frame(0x7F, None)).await;
                    return true;
                }
                if self.opts.oversized_initial {
                    let mut pkt = u32::MAX.to_le_bytes().to_vec();
                    pkt.push(Opcode::Initial as u8);
                    self.send_bytes(&pkt).await;
                    return true;
                }
                let initial = if self.opts.rich_initial {
                    InitialReceiverMessage {
                        display_name: Some("mock".to_owned()),
                        app_name: Some("mock-app".to_owned()),
                        app_version: Some("1.0".to_owned()),
                        play_data: Some(PlayMessage {
                            container: "video/mp4".to_owned(),
                            url: Some("http://localhost/current".to_owned()),
                            content: None,
                            time: Some(5.0),
                            volume: Some(1.0),
                            speed: Some(1.0),
                            headers: None,
                            metadata: None,
                        }),
                        experimental_capabilities: Some(ReceiverCapabilities {
                            av: Some(AVCapabilities {
                                livestream: Some(LivestreamCapabilities { whep: Some(true) }),
                            }),
                        }),
                    }
                } else {
                    InitialReceiverMessage {
                        display_name: Some("mock".to_owned()),
                        ..Default::default()
                    }
                };
                self.send(Opcode::Initial, json(&initial)).await;
                if self.opts.error_after_handshake {
                    self.send(
                        Opcode::PlaybackError,
                        json(&PlaybackErrorMessage {
                            message: "boom".to_owned(),
                        }),
                    )
                    .await;
                }
            }
            Opcode::SubscribeEvent => {
                if let Some(msg) = body
                    .as_deref()
                    .and_then(|b| serde_json::from_slice::<SubscribeEventMessage>(b).ok())
                {
                    let key_variant = match &msg.event {
                        EventSubscribeObject::KeyDown { .. } => Some(EventType::KeyDown),
                        EventSubscribeObject::KeyUp { .. } => Some(EventType::KeyUp),
                        _ => None,
                    };
                    self.state.subs.insert(msg.event);
                    if let Some(variant) = key_variant {
                        let body = json(&EventMessage {
                            generation_time: millis(),
                            event: EventObject::Key {
                                variant,
                                key: "Enter".to_owned(),
                                repeat: false,
                                handled: true,
                            },
                        });
                        self.send(Opcode::Event, body).await;
                    }
                }
            }
            Opcode::UnsubscribeEvent => {
                if let Some(msg) = body
                    .as_deref()
                    .and_then(|b| serde_json::from_slice::<UnsubscribeEventMessage>(b).ok())
                {
                    self.state.subs.remove(&msg.event);
                }
            }
            Opcode::Seek => {
                if let Some(_msg) = body
                    .as_deref()
                    .and_then(|b| serde_json::from_slice::<SeekMessage>(b).ok())
                {
                    self.send_state(PlaybackState::Playing).await;
                }
            }
            Opcode::SetSpeed => {
                if let Some(_msg) = body
                    .as_deref()
                    .and_then(|b| serde_json::from_slice::<SetSpeedMessage>(b).ok())
                {
                    self.send_state(PlaybackState::Playing).await;
                }
            }
            Opcode::SetVolume => {
                if let Some(msg) = body
                    .as_deref()
                    .and_then(|b| serde_json::from_slice::<SetVolumeMessage>(b).ok())
                {
                    self.send(
                        Opcode::VolumeUpdate,
                        json(&VolumeUpdateMessage {
                            generation_time: millis(),
                            volume: msg.volume,
                        }),
                    )
                    .await;
                }
            }
            Opcode::Play => {
                let Some(play) = body
                    .as_deref()
                    .and_then(|b| serde_json::from_slice::<PlayMessage>(b).ok())
                else {
                    return true;
                };

                if self.opts.playback_error_on_play {
                    self.send(
                        Opcode::PlaybackError,
                        json(&PlaybackErrorMessage {
                            message: "play failed".to_owned(),
                        }),
                    )
                    .await;
                    return true;
                }

                let echoed = if self.opts.wrong_play_echo {
                    let mut p = play.clone();
                    p.container.push('X');
                    p
                } else {
                    play.clone()
                };
                self.send(
                    Opcode::PlayUpdate,
                    json(&PlayUpdateMessage {
                        generation_time: Some(millis()),
                        play_data: Some(echoed),
                    }),
                )
                .await;

                let first = match &play.content {
                    Some(content) => match serde_json::from_str::<PlaylistContent>(content) {
                        Ok(playlist) => {
                            let offset = playlist.offset.unwrap_or(0) as usize;
                            self.state.playlist = playlist.items;
                            self.state.playlist.get(offset).cloned()
                        }
                        Err(_) => Some(play.into()),
                    },
                    None => {
                        let mut item: MediaItem = play.into();
                        if self.opts.wrong_event_item {
                            item.container.push('X');
                        }
                        Some(item)
                    }
                };
                if let Some(item) = first {
                    self.emit_item_events(&item).await;
                }
            }
            Opcode::SetPlaylistItem => {
                if let Some(msg) = body
                    .as_deref()
                    .and_then(|b| serde_json::from_slice::<SetPlaylistItemMessage>(b).ok())
                    && let Some(item) = self.state.playlist.get(msg.item_index as usize).cloned()
                {
                    self.emit_item_events(&item).await;
                }
            }
            Opcode::Pause => {
                if self.opts.stale_state {
                    self.send_state(PlaybackState::Playing).await;
                }
                self.send_state(PlaybackState::Paused).await;
            }
            Opcode::Resume => {
                if self.opts.stale_state {
                    self.send_state(PlaybackState::Paused).await;
                }
                self.send_state(PlaybackState::Playing).await;
            }
            Opcode::Stop => {
                self.send_state(PlaybackState::Idle).await;
            }
            _ => {}
        }
        true
    }
}

async fn run(addr: SocketAddr, steps: &[Step]) -> anyhow::Result<()> {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("dummy.bin"), b"dummy media").unwrap();
    let file_server = FileServer::new(0).await.unwrap();
    let media: PathBuf = dir.path().to_path_buf();
    run_case(&addr, &file_server, &media, steps, None).await
}

fn handshake() -> Vec<Step> {
    vec![
        Step::Receive(Receive::Version),
        Step::Send(Send::Version(3)),
        Step::Send(Send::Initial),
        Step::Receive(Receive::Initial),
    ]
}

fn serve(id: u32) -> Step {
    Step::ServeFile {
        path: "dummy.bin",
        id,
        mime: "video/mp4",
        headers: None,
    }
}

fn serve_with_headers(id: u32) -> Step {
    Step::ServeFile {
        path: "dummy.bin",
        id,
        mime: "video/mp4",
        headers: Some(&[("User-Agent", "fast"), ("Authorization", "Bearer x")]),
    }
}

fn sub(event: EventSubscribeObject) -> Step {
    Step::Send(Send::SubscribeEvent(event))
}

fn assert_err_contains(result: anyhow::Result<()>, needle: &str) {
    let err = result.expect_err("expected the test to fail");
    let msg = format!("{err:?}");
    assert!(
        msg.contains(needle),
        "error {msg:?} did not contain {needle:?}"
    );
}

#[tokio::test]
async fn handshake_against_quiet_receiver() {
    let addr = spawn_mock(MockOptions::default()).await;
    run(addr, &handshake())
        .await
        .expect("handshake should succeed");
}

#[tokio::test]
async fn handshake_against_chatty_receiver() {
    let addr = spawn_mock(MockOptions {
        chatty: true,
        ..Default::default()
    })
    .await;
    run(addr, &handshake())
        .await
        .expect("handshake should survive a chatty receiver");
}

#[tokio::test]
async fn handshake_survives_prelude_burst_of_batched_packets() {
    let addr = spawn_mock(MockOptions {
        prelude_burst: true,
        ..Default::default()
    })
    .await;
    run(addr, &handshake())
        .await
        .expect("handshake should survive batched early chatter");
}

#[tokio::test]
async fn handshake_survives_byte_at_a_time_framing() {
    let addr = spawn_mock(MockOptions {
        dribble: true,
        ..Default::default()
    })
    .await;
    run(addr, &handshake())
        .await
        .expect("handshake should survive dribbled framing");
}

#[tokio::test]
async fn single_ping_pong() {
    let addr = spawn_mock(MockOptions::default()).await;
    let mut steps = handshake();
    steps.extend([Step::Send(Send::Ping), Step::Receive(Receive::Pong)]);
    run(addr, &steps).await.expect("ping/pong should succeed");
}

#[tokio::test]
async fn repeated_heartbeat() {
    let addr = spawn_mock(MockOptions::default()).await;
    let mut steps = handshake();
    for _ in 0..3 {
        steps.push(Step::Send(Send::Ping));
        steps.push(Step::Receive(Receive::Pong));
    }
    run(addr, &steps)
        .await
        .expect("repeated heartbeat should succeed");
}

#[tokio::test]
async fn play_v2_flow() {
    let addr = spawn_mock(MockOptions {
        chatty: true,
        ..Default::default()
    })
    .await;
    let steps = vec![
        Step::Receive(Receive::Version),
        Step::Send(Send::Version(2)),
        serve(0),
        Step::Send(Send::PlayV2 { file_id: 0 }),
        Step::SleepMillis(100),
        Step::Send(Send::Stop),
    ];
    run(addr, &steps)
        .await
        .expect("v2 play flow should succeed");
}

#[tokio::test]
async fn play_v3_with_media_item_start_event() {
    let addr = spawn_mock(MockOptions {
        chatty: true,
        ..Default::default()
    })
    .await;
    let mut steps = handshake();
    steps.extend([
        sub(EventSubscribeObject::MediaItemStart),
        serve(0),
        Step::Send(Send::PlayV3 { file_id: 0 }),
        Step::Send(Send::Stop),
    ]);
    run(addr, &steps)
        .await
        .expect("play + start event should succeed");
}

#[tokio::test]
async fn play_v3_with_body_time_speed_volume() {
    let addr = spawn_mock(MockOptions {
        chatty: true,
        ..Default::default()
    })
    .await;
    let mut steps = handshake();
    steps.extend([
        sub(EventSubscribeObject::MediaItemStart),
        serve(0),
        Step::Send(Send::PlayV3WithBody {
            file_id: 0,
            time: Some(12.5),
            volume: Some(0.8),
            speed: Some(1.5),
        }),
        Step::Send(Send::Stop),
    ]);
    run(addr, &steps)
        .await
        .expect("play-with-body should be echoed correctly");
}

#[tokio::test]
async fn play_resolves_start_and_changed_subscriptions_together() {
    let addr = spawn_mock(MockOptions::default()).await;
    let mut steps = handshake();
    steps.extend([
        sub(EventSubscribeObject::MediaItemStart),
        sub(EventSubscribeObject::MediaItemChanged),
        serve(0),
        Step::Send(Send::PlayV3 { file_id: 0 }),
        Step::Send(Send::Stop),
    ]);
    run(addr, &steps)
        .await
        .expect("both event subscriptions should resolve");
}

#[tokio::test]
async fn play_resolves_media_item_end_subscription() {
    let addr = spawn_mock(MockOptions::default()).await;
    let mut steps = handshake();
    steps.extend([
        sub(EventSubscribeObject::MediaItemEnd),
        serve(0),
        Step::Send(Send::PlayV3 { file_id: 0 }),
        Step::Send(Send::Stop),
    ]);
    run(addr, &steps)
        .await
        .expect("end-event subscription should resolve");
}

#[tokio::test]
async fn pause_resume_ignores_stale_state_updates() {
    let addr = spawn_mock(MockOptions {
        chatty: true,
        stale_state: true,
        ..Default::default()
    })
    .await;
    let mut steps = handshake();
    steps.extend([
        serve(0),
        Step::Send(Send::PlayV3 { file_id: 0 }),
        Step::Send(Send::Pause),
        Step::Send(Send::Resume),
    ]);
    run(addr, &steps)
        .await
        .expect("pause/resume should wait for the correct state");
}

#[tokio::test]
async fn volume_update_confirmed() {
    let addr = spawn_mock(MockOptions {
        chatty: true,
        ..Default::default()
    })
    .await;
    let mut steps = handshake();
    steps.push(Step::Send(Send::SetVolume(0.5)));
    run(addr, &steps)
        .await
        .expect("volume update should be confirmed");
}

#[tokio::test]
async fn playlist_and_set_item() {
    let addr = spawn_mock(MockOptions {
        chatty: true,
        ..Default::default()
    })
    .await;
    let mut steps = handshake();
    steps.extend([
        sub(EventSubscribeObject::MediaItemChanged),
        serve(0),
        serve(1),
        Step::Send(Send::PlaylistV3 {
            items: &[PlaylistItem { file_id: 0 }, PlaylistItem { file_id: 1 }],
        }),
        Step::Send(Send::SetPlaylistItem { index: 1 }),
        Step::Send(Send::Stop),
    ]);
    run(addr, &steps)
        .await
        .expect("playlist + set item should succeed");
}

#[tokio::test]
async fn sleep_step_actually_waits() {
    let addr = spawn_mock(MockOptions::default()).await;
    let mut steps = handshake();
    steps.push(Step::SleepMillis(300));

    let start = std::time::Instant::now();
    run(addr, &steps).await.expect("sleep step should succeed");
    assert!(
        start.elapsed() >= Duration::from_millis(280),
        "sleep step did not wait long enough: {:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn missing_expected_packet_fails_instead_of_hanging() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let _ = stream
                .write_all(&frame(
                    Opcode::Version as u8,
                    json(&VersionMessage { version: 3 }).as_deref(),
                ))
                .await;
            let mut buf = [0u8; 1024];
            while !matches!(stream.read(&mut buf).await, Ok(0) | Err(_)) {}
        }
    });

    assert_err_contains(run(addr, &handshake()).await, "timed out");
}

#[tokio::test]
async fn connection_close_mid_test_fails() {
    let addr = spawn_mock(MockOptions {
        close_on_initial: true,
        ..Default::default()
    })
    .await;
    assert_err_contains(run(addr, &handshake()).await, "closed by receiver");
}

#[tokio::test]
async fn playback_error_surfaces_as_failure() {
    let addr = spawn_mock(MockOptions {
        playback_error_on_play: true,
        ..Default::default()
    })
    .await;
    let mut steps = handshake();
    steps.extend([serve(0), Step::Send(Send::PlayV3 { file_id: 0 })]);
    assert_err_contains(run(addr, &steps).await, "play failed");
}

#[tokio::test]
async fn playback_error_during_sleep_is_detected() {
    let addr = spawn_mock(MockOptions {
        error_after_handshake: true,
        ..Default::default()
    })
    .await;
    let mut steps = handshake();
    steps.push(Step::SleepMillis(5000));

    let start = std::time::Instant::now();
    assert_err_contains(run(addr, &steps).await, "boom");
    assert!(
        start.elapsed() < Duration::from_millis(4000),
        "should have failed quickly, took {:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn wrong_play_update_echo_fails() {
    let addr = spawn_mock(MockOptions {
        wrong_play_echo: true,
        ..Default::default()
    })
    .await;
    let mut steps = handshake();
    steps.extend([serve(0), Step::Send(Send::PlayV3 { file_id: 0 })]);
    assert_err_contains(run(addr, &steps).await, "play update");
}

#[tokio::test]
async fn wrong_media_item_event_fails() {
    let addr = spawn_mock(MockOptions {
        wrong_event_item: true,
        ..Default::default()
    })
    .await;
    let mut steps = handshake();
    steps.extend([
        sub(EventSubscribeObject::MediaItemStart),
        serve(0),
        Step::Send(Send::PlayV3 { file_id: 0 }),
    ]);
    assert_err_contains(run(addr, &steps).await, "MediaItemStart event");
}

#[tokio::test]
async fn oversized_packet_fails() {
    let addr = spawn_mock(MockOptions {
        oversized_initial: true,
        ..Default::default()
    })
    .await;
    assert_err_contains(run(addr, &handshake()).await, "oversized");
}

#[tokio::test]
async fn unknown_opcode_fails() {
    let addr = spawn_mock(MockOptions {
        unknown_opcode_initial: true,
        ..Default::default()
    })
    .await;
    assert_err_contains(run(addr, &handshake()).await, "Unknown opcode");
}

#[tokio::test]
async fn malformed_awaited_body_fails() {
    let addr = spawn_mock(MockOptions {
        bad_json_initial: true,
        ..Default::default()
    })
    .await;
    assert_err_contains(run(addr, &handshake()).await, "parsing Initial body");
}

#[tokio::test]
async fn seek_message() {
    let addr = spawn_mock(MockOptions {
        chatty: true,
        ..Default::default()
    })
    .await;
    let mut steps = handshake();
    steps.extend([
        serve(0),
        Step::Send(Send::PlayV3 { file_id: 0 }),
        Step::Send(Send::Seek(42.5)),
        Step::SleepMillis(100),
        Step::Send(Send::Stop),
    ]);
    run(addr, &steps).await.expect("seek should succeed");
}

#[tokio::test]
async fn set_speed_message() {
    let addr = spawn_mock(MockOptions {
        chatty: true,
        ..Default::default()
    })
    .await;
    let mut steps = handshake();
    steps.extend([
        serve(0),
        Step::Send(Send::PlayV3 { file_id: 0 }),
        Step::Send(Send::SetSpeed(2.0)),
        Step::SleepMillis(100),
        Step::Send(Send::Stop),
    ]);
    run(addr, &steps).await.expect("set speed should succeed");
}

#[tokio::test]
async fn subscribe_then_unsubscribe_drops_event_expectation() {
    let addr = spawn_mock(MockOptions::default()).await;
    let mut steps = handshake();
    steps.extend([
        sub(EventSubscribeObject::MediaItemStart),
        Step::Send(Send::UnsubscribeEvent(EventSubscribeObject::MediaItemStart)),
        serve(0),
        Step::Send(Send::PlayV3 { file_id: 0 }),
        Step::Send(Send::Stop),
    ]);
    run(addr, &steps)
        .await
        .expect("unsubscribe should clear the expectation");
}

#[tokio::test]
async fn play_with_metadata() {
    let addr = spawn_mock(MockOptions::default()).await;
    let mut steps = handshake();
    steps.extend([
        sub(EventSubscribeObject::MediaItemStart),
        serve(0),
        Step::Send(Send::PlayV3WithMetadata {
            file_id: 0,
            title: Some("A Title"),
            thumbnail_url: Some("http://localhost/thumb.jpg"),
        }),
        Step::Send(Send::Stop),
    ]);
    run(addr, &steps)
        .await
        .expect("metadata should round-trip through the event");
}

#[tokio::test]
async fn play_inline_content() {
    let addr = spawn_mock(MockOptions::default()).await;
    let mut steps = handshake();
    steps.extend([
        sub(EventSubscribeObject::MediaItemStart),
        Step::Send(Send::PlayContent {
            mime: "application/dash+xml",
            content: "<MPD></MPD>",
        }),
        Step::Send(Send::Stop),
    ]);
    run(addr, &steps)
        .await
        .expect("inline-content play should succeed");
}

#[tokio::test]
async fn playlist_with_options() {
    let addr = spawn_mock(MockOptions::default()).await;
    let mut steps = handshake();
    steps.extend([
        sub(EventSubscribeObject::MediaItemChanged),
        serve(0),
        serve(1),
        serve(2),
        Step::Send(Send::PlaylistV3WithOptions {
            items: &[
                PlaylistItem { file_id: 0 },
                PlaylistItem { file_id: 1 },
                PlaylistItem { file_id: 2 },
            ],
            offset: Some(1),
            volume: Some(0.7),
            speed: Some(1.25),
        }),
        Step::Send(Send::Stop),
    ]);
    run(addr, &steps)
        .await
        .expect("playlist with options should start at the offset");
}

#[tokio::test]
async fn key_event_is_handled_gracefully() {
    // Subscribing to a key event makes the mock fire one; the engine should
    // parse and ignore it without disturbing the run.
    let addr = spawn_mock(MockOptions::default()).await;
    let mut steps = handshake();
    steps.extend([
        sub(EventSubscribeObject::KeyDown {
            keys: vec!["Enter".to_owned()],
        }),
        Step::SleepMillis(150),
    ]);
    run(addr, &steps)
        .await
        .expect("key events should be ignored");
}

#[tokio::test]
async fn v2_format_playback_updates_are_accepted() {
    let addr = spawn_mock(MockOptions {
        v2_playback_format: true,
        ..Default::default()
    })
    .await;
    let mut steps = handshake();
    steps.extend([
        serve(0),
        Step::Send(Send::PlayV3 { file_id: 0 }),
        Step::Send(Send::Pause),
        Step::Send(Send::Resume),
    ]);
    run(addr, &steps)
        .await
        .expect("engine should accept v2-shaped playback updates");
}

#[tokio::test]
async fn initial_with_capabilities_and_play_data() {
    let addr = spawn_mock(MockOptions {
        rich_initial: true,
        ..Default::default()
    })
    .await;
    run(addr, &handshake())
        .await
        .expect("a rich Initial reply should parse");
}

#[tokio::test]
async fn play_with_request_headers_round_trips() {
    let addr = spawn_mock(MockOptions::default()).await;
    let mut steps = handshake();
    steps.extend([
        sub(EventSubscribeObject::MediaItemStart),
        serve_with_headers(0),
        Step::Send(Send::PlayV3 { file_id: 0 }),
        Step::Send(Send::Stop),
    ]);
    run(addr, &steps)
        .await
        .expect("request headers should round-trip through play update + event");
}

#[tokio::test]
async fn play_v2_with_request_headers() {
    let addr = spawn_mock(MockOptions::default()).await;
    let steps = vec![
        Step::Receive(Receive::Version),
        Step::Send(Send::Version(2)),
        serve_with_headers(0),
        Step::Send(Send::PlayV2 { file_id: 0 }),
        Step::SleepMillis(100),
        Step::Send(Send::Stop),
    ];
    run(addr, &steps)
        .await
        .expect("v2 play with headers should succeed");
}

#[derive(Clone, Copy, Default)]
struct V4Opts {
    error_on_volume: Option<v4::flat::ErrorKind>,
    error_on_seek: Option<v4::flat::ErrorKind>,
}

/// Generate a self-signed cert + acceptor (as the real receiver does) and the
/// SHA-256 SPKI fingerprint a sender pins it by.
fn server_tls() -> (TlsAcceptor, Vec<u8>) {
    use rcgen::{CertificateParams, DistinguishedName, KeyPair, PublicKeyData, date_time_ymd};
    use sha2::Digest;
    use tokio_rustls::rustls;

    let mut params: CertificateParams = Default::default();
    params.not_before = date_time_ymd(1975, 1, 1);
    params.not_after = date_time_ymd(4096, 1, 1);
    params.distinguished_name = DistinguishedName::new();
    let key_pair = KeyPair::generate().unwrap();
    let cert = params.self_signed(&key_pair).unwrap();
    let fingerprint = sha2::Sha256::digest(key_pair.subject_public_key_info()).to_vec();

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().to_owned()], key_pair.into())
        .unwrap();

    (TlsAcceptor::from(Arc::new(config)), fingerprint)
}

/// Read exactly one plaintext FCast packet, leaving any following bytes (the
/// TLS ClientHello) untouched in the socket for the acceptor.
async fn read_plain_packet(stream: &mut TcpStream) -> std::io::Result<(u8, Vec<u8>)> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).await?;
    let size = u32::from_le_bytes(len) as usize;
    let mut body = vec![0u8; size];
    stream.read_exact(&mut body).await?;
    let (op, rest) = body.split_first().expect("packet has at least an opcode");
    Ok((*op, rest.to_vec()))
}

async fn send_flat<S>(stream: &mut S, msg: &[u8])
where
    S: AsyncWriteExt + Unpin,
{
    let _ = stream
        .write_all(&frame(Opcode::Flatbuf as u8, Some(msg)))
        .await;
    let _ = stream.flush().await;
}

async fn spawn_mock_v4(opts: V4Opts) -> (SocketAddr, Vec<u8>) {
    let (acceptor, fingerprint) = server_tls();
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((tcp, _)) = listener.accept().await {
            run_mock_v4(tcp, acceptor, opts).await;
        }
    });
    (addr, fingerprint)
}

async fn v4_server_handshake(
    mut tcp: TcpStream,
    acceptor: TlsAcceptor,
) -> Option<tokio_rustls::server::TlsStream<TcpStream>> {
    let _ = tcp
        .write_all(&frame(
            Opcode::Version as u8,
            json(&VersionMessage { version: 4 }).as_deref(),
        ))
        .await;
    let _ = tcp.flush().await;
    let (op, _) = read_plain_packet(&mut tcp).await.ok()?;
    if op != Opcode::Version as u8 {
        return None;
    }

    let mut tls = acceptor.accept(tcp).await.ok()?;

    let info = v4::DeviceInfo {
        display_name: Some("mock".to_owned()),
        app_name: Some("mock".to_owned()),
        app_version: Some("1.0".to_owned()),
    };
    let intro = v4::MessageBuilder::new().receiver_introduction(
        &info,
        std::iter::empty(),
        std::iter::empty(),
        std::iter::empty(),
        std::iter::empty(),
        std::iter::empty(),
        std::iter::empty(),
        std::iter::empty(),
        false,
        0.01,
    );
    send_flat(&mut tls, &intro).await;
    Some(tls)
}

async fn run_mock_v4(tcp: TcpStream, acceptor: TlsAcceptor, opts: V4Opts) {
    let Some(mut tls) = v4_server_handshake(tcp, acceptor).await else {
        return;
    };

    let mut reader = PacketReader::new(512 * 1024, 8192);
    let mut buf = [0u8; 8192];
    loop {
        let Ok(n) = tls.read(&mut buf).await else {
            return;
        };
        if n == 0 || reader.push_data(&buf[..n]).is_err() {
            return;
        }

        let mut packets: Vec<(u8, Vec<u8>)> = Vec::new();
        loop {
            match reader.get_packet() {
                ReadResult::NeedData | ReadResult::PacketTooLarge(_) => break,
                ReadResult::Read(pkt) => {
                    if let Some((&op, rest)) = pkt.split_first() {
                        packets.push((op, rest.to_vec()));
                    }
                }
            }
        }

        for (op, body) in packets {
            match Opcode::try_from(op) {
                Ok(Opcode::Ping) => {
                    let _ = tls.write_all(&frame(Opcode::Pong as u8, None)).await;
                    let _ = tls.flush().await;
                }
                Ok(Opcode::Flatbuf) => handle_flat_v4(&mut tls, &body, &opts).await,
                Ok(_) => {}
                Err(_) => {
                    let msg =
                        v4::MessageBuilder::new().error(None, v4::flat::ErrorKind::InvalidOpcode);
                    send_flat(&mut tls, &msg).await;
                }
            }
        }
    }
}

async fn handle_flat_v4<S>(tls: &mut S, body: &[u8], opts: &V4Opts)
where
    S: AsyncWriteExt + Unpin,
{
    use v4::flat::Message;
    let Ok(packet) = v4::flat::root_as_packet(body) else {
        return;
    };

    match packet.payload_type() {
        Message::VolumeChanged => {
            let vol = packet.payload_as_volume_changed().unwrap().volume();
            let msg = match opts.error_on_volume {
                Some(kind) => v4::MessageBuilder::new().error(None, kind),
                None => v4::MessageBuilder::new().volume_changed(vol),
            };
            send_flat(tls, &msg).await;
        }
        Message::SpeedChanged => {
            let speed = packet.payload_as_speed_changed().unwrap().speed();
            let msg = v4::MessageBuilder::new().speed_changed(speed);
            send_flat(tls, &msg).await;
        }
        Message::PlaybackStateChanged => {
            let state = packet.payload_as_playback_state_changed().unwrap().state();
            let msg = v4::MessageBuilder::new().playback_state_changed(state);
            send_flat(tls, &msg).await;
        }
        Message::ProgressChanged => {
            if let Some(kind) = opts.error_on_seek {
                let msg = v4::MessageBuilder::new().error(None, kind);
                send_flat(tls, &msg).await;
            }
        }
        // Pretend playback started so a `PlayV4` looks alive.
        Message::Load => {
            let msg =
                v4::MessageBuilder::new().playback_state_changed(v4::flat::PlaybackState::Playing);
            send_flat(tls, &msg).await;
        }
        _ => {}
    }
}

async fn run_v4(addr: SocketAddr, fingerprint: Vec<u8>, steps: &[Step]) -> anyhow::Result<()> {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("dummy.bin"), b"dummy media").unwrap();
    let file_server = FileServer::new(0).await.unwrap();
    let media: PathBuf = dir.path().to_path_buf();
    run_case(&addr, &file_server, &media, steps, Some(fingerprint)).await
}

fn v4_handshake() -> Vec<Step> {
    vec![
        Step::Receive(Receive::Version),
        Step::Send(Send::Version(4)),
        Step::Send(Send::SenderIntroduction),
        Step::Receive(Receive::ReceiverIntroduction),
    ]
}

#[tokio::test]
async fn v4_handshake_and_introduction() {
    let (addr, fp) = spawn_mock_v4(V4Opts::default()).await;
    run_v4(addr, fp, &v4_handshake())
        .await
        .expect("v4 handshake + TLS upgrade + introduction should succeed");
}

#[tokio::test]
async fn v4_heartbeat() {
    let (addr, fp) = spawn_mock_v4(V4Opts::default()).await;
    let mut steps = v4_handshake();
    for _ in 0..3 {
        steps.push(Step::Send(Send::Ping));
        steps.push(Step::Receive(Receive::Pong));
    }
    run_v4(addr, fp, &steps)
        .await
        .expect("v4 heartbeat over TLS should succeed");
}

#[tokio::test]
async fn v4_play_and_stop() {
    let (addr, fp) = spawn_mock_v4(V4Opts::default()).await;
    let mut steps = v4_handshake();
    steps.extend([
        serve(0),
        Step::Send(Send::PlayV4 { file_id: 0 }),
        Step::SleepMillis(100),
        Step::Send(Send::StopV4),
    ]);
    run_v4(addr, fp, &steps)
        .await
        .expect("v4 load + stop should succeed");
}

#[tokio::test]
async fn v4_queue_load_and_modify() {
    let (addr, fp) = spawn_mock_v4(V4Opts::default()).await;
    let mut steps = v4_handshake();
    steps.extend([
        serve(0),
        serve(1),
        Step::Send(Send::LoadQueueV4 {
            items: &[PlaylistItem { file_id: 0 }, PlaylistItem { file_id: 1 }],
            start_index: Some(0),
        }),
        Step::SleepMillis(100),
        Step::Send(Send::QueueInsertV4 {
            file_id: 1,
            position: v4::QueuePosition::Back,
        }),
        Step::Send(Send::QueueRemoveV4 {
            position: v4::QueuePosition::Index(0),
        }),
        Step::Send(Send::QueueSelectV4 {
            position: v4::QueuePosition::Front,
        }),
        Step::SleepMillis(100),
        Step::Send(Send::StopV4),
    ]);
    run_v4(addr, fp, &steps)
        .await
        .expect("v4 queue load + modifications should succeed");
}

#[tokio::test]
async fn v4_volume_changed_is_confirmed() {
    let (addr, fp) = spawn_mock_v4(V4Opts::default()).await;
    let mut steps = v4_handshake();
    steps.extend([
        Step::Send(Send::SetVolumeV4(0.5)),
        Step::Send(Send::SetVolumeV4(1.0)),
    ]);
    run_v4(addr, fp, &steps)
        .await
        .expect("v4 volume changes should be echoed back");
}

#[tokio::test]
async fn v4_speed_changed_is_confirmed() {
    let (addr, fp) = spawn_mock_v4(V4Opts::default()).await;
    let mut steps = v4_handshake();
    steps.push(Step::Send(Send::SetSpeedV4(1.5)));
    run_v4(addr, fp, &steps)
        .await
        .expect("v4 speed change should be echoed back");
}

#[tokio::test]
async fn v4_pause_resume_is_confirmed() {
    let (addr, fp) = spawn_mock_v4(V4Opts::default()).await;
    let mut steps = v4_handshake();
    steps.extend([
        serve(0),
        Step::Send(Send::PlayV4 { file_id: 0 }),
        Step::Send(Send::PauseV4),
        Step::Send(Send::ResumeV4),
        Step::Send(Send::StopV4),
    ]);
    run_v4(addr, fp, &steps)
        .await
        .expect("v4 pause/resume should be confirmed via PlaybackStateChanged");
}

#[tokio::test]
async fn v4_unexpected_error_surfaces_as_failure() {
    let (addr, fp) = spawn_mock_v4(V4Opts {
        error_on_volume: Some(v4::flat::ErrorKind::InvalidOpcode),
        ..Default::default()
    })
    .await;
    let mut steps = v4_handshake();
    steps.push(Step::Send(Send::SetVolumeV4(0.5)));
    assert_err_contains(run_v4(addr, fp, &steps).await, "InvalidOpcode");
}

#[tokio::test]
async fn v4_expected_error_is_accepted() {
    let (addr, fp) = spawn_mock_v4(V4Opts {
        error_on_seek: Some(v4::flat::ErrorKind::SeekOutOfRange),
        ..Default::default()
    })
    .await;
    let mut steps = v4_handshake();
    steps.extend([
        Step::Send(Send::SeekV4(99_999.0)),
        Step::Receive(Receive::Error(v4::flat::ErrorKind::SeekOutOfRange)),
    ]);
    run_v4(addr, fp, &steps)
        .await
        .expect("an expected v4 error should satisfy the expectation");
}

#[tokio::test]
async fn v4_wrong_error_kind_fails() {
    let (addr, fp) = spawn_mock_v4(V4Opts {
        error_on_seek: Some(v4::flat::ErrorKind::VolumeOutOfRange),
        ..Default::default()
    })
    .await;
    let mut steps = v4_handshake();
    steps.extend([
        Step::Send(Send::SeekV4(99_999.0)),
        Step::Receive(Receive::Error(v4::flat::ErrorKind::SeekOutOfRange)),
    ]);
    assert_err_contains(run_v4(addr, fp, &steps).await, "VolumeOutOfRange");
}

#[tokio::test]
async fn v4_invalid_opcode_is_rejected() {
    let (addr, fp) = spawn_mock_v4(V4Opts::default()).await;
    let mut steps = v4_handshake();
    steps.extend([
        Step::Send(Send::RawOpcode(0x7F)),
        Step::Receive(Receive::Error(v4::flat::ErrorKind::InvalidOpcode)),
    ]);
    run_v4(addr, fp, &steps)
        .await
        .expect("invalid opcode should be answered with Error(InvalidOpcode)");
}

#[tokio::test]
async fn v4_wrong_fingerprint_aborts_the_upgrade() {
    let (addr, _real_fp) = spawn_mock_v4(V4Opts::default()).await;
    // Pin a fingerprint that does not match the receiver's certificate.
    let bogus = vec![0u8; 32];
    assert_err_contains(run_v4(addr, bogus, &v4_handshake()).await, "TLS");
}

async fn next_packet<S>(
    tls: &mut S,
    reader: &mut PacketReader,
    buf: &mut [u8],
) -> Option<(u8, Vec<u8>)>
where
    S: AsyncReadExt + Unpin,
{
    loop {
        let ready = match reader.get_packet() {
            ReadResult::Read(pkt) => Some(pkt.to_vec()),
            ReadResult::PacketTooLarge(_) => return None,
            ReadResult::NeedData => None,
        };
        match ready {
            Some(pkt) => {
                let (op, rest) = pkt.split_first()?;
                return Some((*op, rest.to_vec()));
            }
            None => {
                let n = tls.read(buf).await.ok()?;
                if n == 0 {
                    return None;
                }
                reader.push_data(&buf[..n]).ok()?;
            }
        }
    }
}

fn parse_fcomp_resource_id(url: &str) -> u32 {
    url.rsplit('/')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

async fn spawn_companion_mock(
    provider_id: u16,
) -> (SocketAddr, Vec<u8>, tokio::sync::oneshot::Receiver<Vec<u8>>) {
    let (acceptor, fingerprint) = server_tls();
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        if let Ok((tcp, _)) = listener.accept().await {
            run_companion_mock(tcp, acceptor, provider_id, tx).await;
        }
    });
    (addr, fingerprint, rx)
}

async fn run_companion_mock(
    tcp: TcpStream,
    acceptor: TlsAcceptor,
    provider_id: u16,
    result_tx: tokio::sync::oneshot::Sender<Vec<u8>>,
) {
    use v4::flat::Message;

    let Some(mut tls) = v4_server_handshake(tcp, acceptor).await else {
        return;
    };
    let mut reader = PacketReader::new(512 * 1024, 8192);
    let mut buf = [0u8; 8192];

    let mut pending: Option<(u32, u32)> = None;
    let mut received: Vec<u8> = Vec::new();

    while let Some((op, body)) = next_packet(&mut tls, &mut reader, &mut buf).await {
        match Opcode::try_from(op) {
            Ok(Opcode::Flatbuf) => {
                let Ok(packet) = v4::flat::root_as_packet(&body) else {
                    continue;
                };
                match packet.payload_type() {
                    Message::CompanionHelloRequest => {
                        let msg = v4::MessageBuilder::new().companion_hello_response(provider_id);
                        send_flat(&mut tls, &msg).await;
                    }
                    Message::Load => {
                        let resource_id = packet
                            .payload_as_load()
                            .and_then(|l| l.source_as_single())
                            .map(|s| parse_fcomp_resource_id(s.source_url()))
                            .unwrap_or(0);
                        let request_id = 1;
                        pending = Some((request_id, resource_id));
                        let msg = v4::MessageBuilder::new()
                            .companion_resource_info_request(request_id, resource_id);
                        send_flat(&mut tls, &msg).await;
                    }
                    Message::CompanionResourceInfoResponse => {
                        if let Some((request_id, resource_id)) = pending {
                            let msg = v4::MessageBuilder::new().companion_resource_request(
                                request_id,
                                resource_id,
                                None,
                            );
                            send_flat(&mut tls, &msg).await;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Opcode::Resource) => {
                let Ok(resp) = companion::ResourceResponse::parse(&body) else {
                    continue;
                };
                if let companion::GetResourceResult::Success(bytes) = resp.result {
                    received.extend_from_slice(&bytes);
                }
                if resp.part == resp.total_parts {
                    let _ = result_tx.send(received);
                    return;
                }
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn v4_companion_resource_is_served() {
    const DATA: &[u8] =
        b"FCompanion resource payload served over the FCast connection \x00\x01\x02";
    const RESOURCE_ID: u32 = 7;
    const PROVIDER_ID: u16 = 3;

    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("res.bin"), DATA).unwrap();
    let file_server = FileServer::new(0).await.unwrap();
    let media: PathBuf = dir.path().to_path_buf();

    let (addr, fp, rx) = spawn_companion_mock(PROVIDER_ID).await;
    let mut steps = v4_handshake();
    steps.extend([
        Step::Send(Send::CompanionHello),
        Step::Send(Send::ServeCompanionFile {
            resource_id: RESOURCE_ID,
            path: "res.bin",
            mime: "application/octet-stream",
        }),
        Step::Send(Send::PlayCompanion {
            resource_id: RESOURCE_ID,
        }),
    ]);

    run_case(&addr, &file_server, &media, &steps, Some(fp))
        .await
        .expect("companion resource flow should succeed");

    let received = rx.await.expect("mock should have reassembled the resource");
    assert_eq!(received, DATA);
}
