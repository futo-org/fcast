mod companion_owner {
    use super::super::*;

    fn item(url: &str, thumb: Option<&str>) -> v4::MediaItem {
        v4::MediaItem {
            container: "video/mp4".into(),
            source_url: url.into(),
            start_time: None,
            volume: None,
            speed: None,
            headers: None,
            title: None,
            thumbnail_url: thumb.map(Into::into),
            metadata: None,
            extra_metadata: None,
        }
    }

    fn load(bytes: &[u8]) -> Operation {
        Operation::PlayNew(WrappedPlayMessage::V4(
            FlatLoadMessage::try_new(bytes.to_vec(), |buf| {
                v4::flat::root_as_packet(buf)
                    .ok()
                    .and_then(|p| p.payload_as_load())
                    .ok_or(())
            })
            .unwrap(),
        ))
    }

    #[test]
    fn only_the_owner_may_name_its_companion_resources() {
        let single =
            |u: &str, t: Option<&str>| load(&v4::MessageBuilder::new().load_single(item(u, t)));
        assert!(!names_foreign_companion(
            &single("fcomp://3.fcast/1", None),
            Some(3)
        ));
        assert!(names_foreign_companion(
            &single("fcomp://4.fcast/1", None),
            Some(3)
        ));
        assert!(names_foreign_companion(
            &single("fcomp://3.fcast/1", None),
            None
        ));
        assert!(!names_foreign_companion(
            &single("http://h/a.mp4", None),
            None
        ));
        assert!(names_foreign_companion(
            &single("http://h/a.mp4", Some("fcomp://9.fcast/2")),
            Some(3)
        ));
        let queue = load(
            &v4::MessageBuilder::new().load_queue(
                [
                    item("fcomp://3.fcast/1", None),
                    item("fcomp://5.fcast/1", None),
                ]
                .into_iter()
                .map(|i| (i, None)),
                None,
                true,
            ),
        );
        assert!(names_foreign_companion(&queue, Some(3)));
        let sub = |url: &str| Operation::AddSubtitleSource {
            url: url.into(),
            select: true,
            name: None,
        };
        assert!(names_foreign_companion(&sub("fcomp://7.fcast/1"), Some(3)));
        assert!(!names_foreign_companion(&sub("fcomp://3.fcast/1"), Some(3)));
        assert!(!names_foreign_companion(&Operation::Pause, None));
    }
}

mod rate {
    use super::super::*;

    #[test]
    fn rates_are_kept_in_the_pipelines_range() {
        assert_eq!(sanitize_rate(1.5), (1.5, true));
        assert_eq!(sanitize_rate(MAX_RATE), (MAX_RATE, true));
        assert_eq!(sanitize_rate(0.0), (1.0, false));
        assert_eq!(sanitize_rate(-1.0), (1.0, false));
        assert_eq!(sanitize_rate(f32::NAN), (1.0, false));
        assert_eq!(sanitize_rate(f32::INFINITY), (1.0, false));
        assert_eq!(sanitize_rate(1e30), (MAX_RATE, false));
        assert_eq!(sanitize_rate(1e-9), (MIN_RATE, false));
    }
}

mod op_budget {
    use super::super::{budgeted_as_transport, Action, OpBudget, Operation};
    use std::time::{Duration, Instant};

    #[test]
    fn every_action_that_queues_work_is_budgeted() {
        assert_eq!(
            budgeted_as_transport(&Action::Op(Operation::Stop)),
            Some(true)
        );
        assert_eq!(
            budgeted_as_transport(&Action::Op(Operation::SetVolume(0.5))),
            Some(false)
        );
        let mirror = Action::StartMirroringSession { session_id: 1 };
        assert_eq!(budgeted_as_transport(&mirror), Some(false));
        assert_eq!(budgeted_as_transport(&Action::Ping), None);
    }

    #[test]
    fn a_flood_is_cut_to_the_rate_and_a_drag_is_not() {
        let t0 = Instant::now();
        let mut b = OpBudget::new(t0);
        let burst = (0..10_000).filter(|_| b.take(t0)).count();
        assert_eq!(burst, OpBudget::BURST as usize);
        // a second later the bucket refilled by one second's worth
        let t1 = t0 + Duration::from_secs(1);
        let refilled = (0..10_000).filter(|_| b.take(t1)).count();
        assert_eq!(refilled, OpBudget::RATE as usize);

        // a 60 Hz slider drag for ten seconds never runs dry
        let mut b = OpBudget::new(t0);
        let ok = (0..600)
            .map(|i| t0 + Duration::from_micros(i * 16_667))
            .all(|t| b.take(t));
        assert!(ok);
    }
}

mod state {
    use super::super::*;

    fn initial_with(url: Option<&str>, content_len: usize) -> v3::InitialReceiverMessage {
        v3::InitialReceiverMessage {
            display_name: Some("Receiver".into()),
            play_data: Some(v3::PlayMessage {
                container: "application/json".into(),
                url: url.map(Into::into),
                content: Some("x".repeat(content_len)),
                time: Some(12.0),
                volume: None,
                speed: None,
                headers: None,
                metadata: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn small_initial_is_sent_whole() {
        let mut initial = initial_with(Some("http://h/p.json"), 100);
        let before = initial.clone();
        fit_initial(&mut initial);
        assert_eq!(initial, before);
    }

    #[test]
    fn oversized_initial_keeps_the_url() {
        let mut initial = initial_with(Some("http://h/p.json"), 100_000);
        fit_initial(&mut initial);
        let play = initial.play_data.as_ref().expect("the url still fits");
        assert_eq!(play.url.as_deref(), Some("http://h/p.json"));
        assert_eq!(play.content, None);
        assert_eq!(play.time, Some(12.0));
        assert!(Packet::Initial(initial).encode().is_ok());
    }

    #[test]
    fn oversized_content_only_initial_drops_the_item() {
        let mut initial = initial_with(None, 100_000);
        fit_initial(&mut initial);
        assert_eq!(initial.play_data, None);
        assert_eq!(initial.display_name.as_deref(), Some("Receiver"));
        assert!(Packet::Initial(initial).encode().is_ok());
    }

    #[test]
    fn oversized_packet_is_an_error_not_a_panic() {
        assert!(Packet::Initial(initial_with(None, 100_000))
            .encode()
            .is_err());
    }

    #[test]
    fn v3_seek_times_outside_the_clock_range_are_refused() {
        assert_eq!(seek_duration(1.5), Some(Duration::from_millis(1500)));
        assert_eq!(seek_duration(0.0), Some(Duration::ZERO));
        assert_eq!(seek_duration(-1.0), None);
        assert_eq!(seek_duration(f64::NAN), None);
        assert_eq!(seek_duration(f64::INFINITY), None);
        // 1e21 ns is past u64
        assert_eq!(seek_duration(1e12), None);
    }

    #[test]
    fn progress_interval_rounding() {
        let ms = |m: u64| Duration::from_millis(m);
        // Sub-minimum requests clamp up to 100ms.
        assert_eq!(round_progress_interval(0), ms(100));
        assert_eq!(round_progress_interval(1), ms(100));
        assert_eq!(round_progress_interval(49_000), ms(100));
        // Rounds to the nearest 100ms.
        assert_eq!(round_progress_interval(50_000), ms(100));
        assert_eq!(round_progress_interval(99_000), ms(100));
        assert_eq!(round_progress_interval(100_000), ms(100));
        assert_eq!(round_progress_interval(120_000), ms(100));
        assert_eq!(round_progress_interval(149_000), ms(100));
        assert_eq!(round_progress_interval(150_000), ms(200));
        assert_eq!(round_progress_interval(180_000), ms(200));
        assert_eq!(round_progress_interval(249_000), ms(200));
        assert_eq!(round_progress_interval(250_000), ms(300));
        assert_eq!(round_progress_interval(500_000), ms(500));
        assert_eq!(round_progress_interval(549_000), ms(500));
        assert_eq!(round_progress_interval(550_000), ms(600));
    }

    fn run_advancements(state: &mut State, events: Vec<(Event, Result<Action, StateError>)>) {
        for (event, res) in events.into_iter() {
            assert_eq!(state.advance(event), res);
        }
    }

    #[test]
    fn timeout() {
        let mut state = State::new();
        let mut events = Vec::new();
        for _ in 0..TICKS_BEFORE_PING - 1 {
            events.push((Event::Tick, Ok(Action::None)));
        }
        events.push((Event::Tick, Ok(Action::Ping)));
        for _ in 0..TICKS_BEFORE_PING - 1 {
            events.push((Event::Tick, Ok(Action::None)));
        }
        events.push((Event::Tick, Ok(Action::EndSession)));
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
                Some(Bytes::copy_from_slice(v2_json.as_bytes())),
                Action::SendVolume,
                SessionVersion::V2,
            ),
            (
                Opcode::Version,
                Some(Bytes::copy_from_slice(v3_json.as_bytes())),
                Action::SendInitial,
                SessionVersion::V3,
            ),
        ];

        for (opcode, body, res, version) in sessions {
            let mut state = State::new();
            run_advancements(&mut state, vec![(Event::Packet { opcode, body }, Ok(res))]);
            assert_eq!(state.variant, StateVariant::Active { version });
        }
    }

    #[test]
    fn invalid_json() {
        let mut state = State::new();
        run_advancements(
            &mut state,
            vec![(
                Event::Packet {
                    opcode: Opcode::Version,
                    body: Some(Bytes::from_static(b"{")),
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
                    Event::Packet {
                        opcode: Opcode::Version,
                        body: Some(Bytes::copy_from_slice(v2_json.as_bytes())),
                    },
                    Ok(Action::SendVolume),
                ),
                (
                    Event::Packet {
                        opcode: Opcode::Initial,
                        body: None,
                    },
                    Err(StateError::IllegalOpcode(Opcode::Initial)),
                ),
            ],
        );
    }

    fn v4_state() -> State {
        let mut state = State::new();
        state.tls_success();
        assert!(matches!(
            state.variant,
            StateVariant::Active {
                version: SessionVersion::V4 { .. }
            }
        ));
        state
    }

    fn advance_flatbuf(state: &mut State, body: &[u8]) -> Result<Action, StateError> {
        state.advance(Event::Packet {
            opcode: Opcode::Flatbuf,
            body: Some(Bytes::copy_from_slice(body)),
        })
    }

    #[test]
    fn v4_resource_payload_reaches_the_companion_channel_without_a_copy() {
        // A companion resource response is media: up to half a megabyte per packet, on
        // its way to a decoder. The driver must hand the received bytes on by
        // sharing them, not by copying them out of the packet - that copy was
        // 10% of the receiver's allocation volume under a seek storm.
        let mut state = v4_state();

        let mut packet = Vec::new();
        packet.extend_from_slice(&7u32.to_le_bytes()); // request_id
        packet.push(0); // part
        packet.push(1); // total_parts
        packet.push(0x01); // GetResourceResult::Success
        let payload_offset = packet.len();
        packet.extend_from_slice(&[0x5Au8; 64 * 1024]);

        let body = Bytes::from(packet);
        let payload_ptr = body[payload_offset..].as_ptr();

        let res = state.advance(Event::Packet {
            opcode: Opcode::Resource,
            body: Some(body),
        });

        let Ok(Action::Companion(CompanionResponse::ResourceResponse(resp))) = res else {
            panic!("expected a companion resource response, got {res:?}");
        };
        let companion::GetResourceResult::Success(chunk) = resp.result else {
            panic!("expected a success result");
        };

        assert_eq!(chunk.len(), 64 * 1024);
        assert!(
            std::ptr::eq(chunk.as_ptr(), payload_ptr),
            "resource payload was copied on its way to the companion channel"
        );
    }

    #[test]
    fn v4_progress_changed_huge_micros_does_not_panic() {
        let mut state = v4_state();
        let msg = v4::MessageBuilder::new()
            .progress_changed_raw(Some(&v4::flat::Time::new(u64::MAX)), None);
        let res = advance_flatbuf(&mut state, &msg);
        assert_eq!(
            res,
            Ok(Action::Error {
                kind: v4::flat::ErrorKind::MalformedBody
            }),
            "expected a graceful error, not a panic"
        );
    }

    fn key_event(variant: v3::EventType, key: &str) -> Arc<ReceiverToSenderMessage> {
        Arc::new(ReceiverToSenderMessage::Event {
            msg: v3::EventMessage {
                generation_time: 0,
                event: v3::EventObject::Key {
                    variant,
                    key: key.to_owned(),
                    repeat: false,
                    handled: false,
                },
            },
        })
    }

    #[test]
    fn v3_keyup_subscription_forwards_keyup() {
        let mut state = State::new();
        state.variant = StateVariant::Active {
            version: SessionVersion::V3,
        };
        state.key_name_events_up = KeyEventFlags::Enter;

        let res = state.advance(Event::ToSender(key_event(v3::EventType::KeyUp, "Enter")));

        assert!(
            matches!(res, Ok(Action::Forward { .. })),
            "KeyUp subscriber should receive KeyUp events, got {res:?}"
        );
    }

    #[test]
    fn v3_keydown_only_does_not_forward_keyup() {
        let mut state = State::new();
        state.variant = StateVariant::Active {
            version: SessionVersion::V3,
        };
        state.key_name_events_down = KeyEventFlags::Enter;

        let res = state.advance(Event::ToSender(key_event(v3::EventType::KeyUp, "Enter")));

        assert_eq!(
            res,
            Ok(Action::None),
            "KeyDown-only subscriber should not receive KeyUp events"
        );
    }

    #[test]
    fn v4_progress_changed_normal_micros_seeks() {
        let mut state = v4_state();

        let micros = 5_000_000u64;
        let msg = v4::MessageBuilder::new()
            .progress_changed_raw(Some(&v4::flat::Time::new(micros)), None);

        let res = advance_flatbuf(&mut state, &msg);

        assert_eq!(
            res,
            Ok(Action::Op(Operation::Seek(Duration::from_micros(micros))))
        );
    }
}
