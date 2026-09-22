//! End-to-end pump test driven by a canned transport that replays UMP bytes.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use prost::Message;
use sabrump::{
    PartType, SabrFormat, SabrSession, SabrSessionEvent, SabrStreamSpec, SabrTransport,
    proto::{
        ByteRange, FormatId, FormatInitializationMetadata, LiveMetadata, MediaHeader, MediaType,
        NextRequestPolicy, SabrSeek, VideoPlaybackAbrRequest,
    },
    spec::Role,
};

const ITAG: i32 = 137;
const LMT: u64 = 1_700_000_000;

// --- UMP encoding helpers ---

fn write_varint(out: &mut Vec<u8>, value: u64) {
    if value < 128 {
        out.push(value as u8);
    } else if value < 1 << 14 {
        out.push(0x80 | (value & 0x3F) as u8);
        out.push(((value >> 6) & 0xFF) as u8);
    } else if value < 1 << 21 {
        out.push(0xC0 | (value & 0x1F) as u8);
        out.push(((value >> 5) & 0xFF) as u8);
        out.push(((value >> 13) & 0xFF) as u8);
    } else {
        out.push(0xE0 | (value & 0x0F) as u8);
        out.push(((value >> 4) & 0xFF) as u8);
        out.push(((value >> 12) & 0xFF) as u8);
        out.push(((value >> 20) & 0xFF) as u8);
    }
}

fn ump_part(out: &mut Vec<u8>, ty: PartType, data: &[u8]) {
    write_varint(out, ty.to_wire() as u64);
    write_varint(out, data.len() as u64);
    out.extend_from_slice(data);
}

/// Emit a full segment: MEDIA_HEADER, MEDIA, MEDIA_END.
#[allow(clippy::too_many_arguments)]
fn emit_segment(
    out: &mut Vec<u8>,
    itag: i32,
    lmt: u64,
    header_id: i32,
    sequence: i32,
    is_init: bool,
    start_ms: i64,
    duration_ms: i64,
    payload: &[u8],
) {
    let header = MediaHeader {
        header_id,
        itag,
        lmt,
        is_init_segment: is_init,
        sequence_number: sequence,
        start_ms,
        duration_ms,
        content_length: payload.len() as i64,
        ..Default::default()
    };
    ump_part(out, PartType::MediaHeader, &header.encode_to_vec());

    let mut media = Vec::new();
    write_varint(&mut media, header_id as u64);
    media.extend_from_slice(payload);
    ump_part(out, PartType::Media, &media);

    let mut end = Vec::new();
    write_varint(&mut end, header_id as u64);
    ump_part(out, PartType::MediaEnd, &end);
}

fn build_response() -> Vec<u8> {
    let mut out = Vec::new();

    let init = FormatInitializationMetadata {
        video_id: "vid".into(),
        format_id: Some(FormatId {
            itag: ITAG,
            lmt: LMT,
            xtags: String::new(),
        }),
        mime_type: "video/mp4; codecs=\"avc1.640028\"".into(),
        end_time_ms: 3000,
        end_segment_number: 2,
        init_range: Some(ByteRange { start: 0, end: 4 }),
        index_range: Some(ByteRange { start: 4, end: 8 }),
        ..Default::default()
    };
    ump_part(
        &mut out,
        PartType::FormatInitializationMetadata,
        &init.encode_to_vec(),
    );

    emit_segment(&mut out, ITAG, LMT, 1, 0, true, 0, 0, b"INIT");
    emit_segment(&mut out, ITAG, LMT, 2, 0, false, 0, 1000, b"SEG0-data");
    emit_segment(&mut out, ITAG, LMT, 3, 1, false, 1000, 1000, b"SEG1-data");
    emit_segment(&mut out, ITAG, LMT, 4, 2, false, 2000, 1000, b"SEG2-data");

    let policy = NextRequestPolicy {
        target_video_readahead_ms: 10_000,
        playback_cookie: b"cookie".to_vec(),
        ..Default::default()
    };
    ump_part(
        &mut out,
        PartType::NextRequestPolicy,
        &policy.encode_to_vec(),
    );

    out
}

fn video_format() -> SabrFormat {
    SabrFormat {
        itag: ITAG,
        last_modified: LMT,
        xtags: String::new(),
        mime_type: "video/mp4; codecs=\"avc1.640028\"".into(),
        codecs: "avc1.640028".into(),
        bitrate: 2_500_000,
        width: 1920,
        height: 1080,
        fps: 30,
        audio_channels: 0,
        audio_sample_rate: 0,
        language: None,
        is_original_audio: false,
        is_drc: false,
    }
}

fn spec() -> SabrStreamSpec {
    SabrStreamSpec {
        server_abr_streaming_url: "https://example.test/videoplayback".into(),
        ustreamer_config: vec![1, 2, 3],
        video_id: "vid".into(),
        is_live: false,
        duration_us: 3_000_000,
        video_formats: vec![video_format()],
        audio_formats: vec![],
        po_token: None,
        client_name: 1,
        client_version: "2.0".into(),
        os_name: "Linux".into(),
        os_version: "6".into(),
    }
}

// --- audio-only fixtures ---

const AUDIO_ITAG: i32 = 140;
const AUDIO_LMT: u64 = 1_700_000_001;

fn audio_format() -> SabrFormat {
    SabrFormat {
        itag: AUDIO_ITAG,
        last_modified: AUDIO_LMT,
        xtags: String::new(),
        mime_type: "audio/mp4; codecs=\"mp4a.40.2\"".into(),
        codecs: "mp4a.40.2".into(),
        bitrate: 128_000,
        width: 0,
        height: 0,
        fps: 0,
        audio_channels: 2,
        audio_sample_rate: 44_100,
        language: None,
        is_original_audio: true,
        is_drc: false,
    }
}

fn audio_spec() -> SabrStreamSpec {
    SabrStreamSpec {
        video_formats: vec![],
        audio_formats: vec![audio_format()],
        ..spec()
    }
}

/// An audio-only response. `exact` toggles whether the media headers carry a
/// concrete duration. When false, duration is zero, forcing the estimate path.
fn build_audio_response(exact: bool) -> Vec<u8> {
    let mut out = Vec::new();

    let init = FormatInitializationMetadata {
        video_id: "vid".into(),
        format_id: Some(FormatId {
            itag: AUDIO_ITAG,
            lmt: AUDIO_LMT,
            xtags: String::new(),
        }),
        mime_type: "audio/mp4; codecs=\"mp4a.40.2\"".into(),
        end_time_ms: 3000,
        end_segment_number: 2,
        ..Default::default()
    };
    ump_part(
        &mut out,
        PartType::FormatInitializationMetadata,
        &init.encode_to_vec(),
    );

    let dur = if exact { 1000 } else { 0 };
    emit_segment(&mut out, AUDIO_ITAG, AUDIO_LMT, 1, 0, true, 0, 0, b"AINIT");
    emit_segment(
        &mut out,
        AUDIO_ITAG,
        AUDIO_LMT,
        2,
        0,
        false,
        0,
        dur,
        b"AUDIO-SEG0",
    );
    emit_segment(
        &mut out,
        AUDIO_ITAG,
        AUDIO_LMT,
        3,
        1,
        false,
        1000,
        dur,
        b"AUDIO-SEG1",
    );
    emit_segment(
        &mut out,
        AUDIO_ITAG,
        AUDIO_LMT,
        4,
        2,
        false,
        2000,
        dur,
        b"AUDIO-SEG2",
    );

    let policy = NextRequestPolicy {
        target_audio_readahead_ms: 10_000,
        playback_cookie: b"cookie".to_vec(),
        ..Default::default()
    };
    ump_part(
        &mut out,
        PartType::NextRequestPolicy,
        &policy.encode_to_vec(),
    );

    out
}

// --- live fixtures ---

fn live_spec() -> SabrStreamSpec {
    SabrStreamSpec {
        is_live: true,
        duration_us: 0,
        video_formats: vec![video_format()],
        audio_formats: vec![],
        ..spec()
    }
}

/// A `LiveMetadata` part with a `[0s, 10s]` seekable window and head at 10s.
fn live_metadata_part(out: &mut Vec<u8>) {
    let lm = LiveMetadata {
        head_sequence_number: 105,
        head_sequence_time_ms: 10_000,
        min_seekable_time_ticks: 0,
        min_seekable_timescale: 1000,
        max_seekable_time_ticks: 10_000,
        max_seekable_timescale: 1000,
        ..Default::default()
    };
    ump_part(out, PartType::LiveMetadata, &lm.encode_to_vec());
}

fn sabr_seek_part(out: &mut Vec<u8>, media_time: i64, timescale: i32) {
    let seek = SabrSeek {
        seek_media_time: media_time,
        seek_media_timescale: timescale,
        seek_source: 11,
    };
    ump_part(out, PartType::SabrSeek, &seek.encode_to_vec());
}

/// The join response of the live-wedge scenario: window `[0s,10s]`, segments
/// 100 (1s..6s) and 101 (6s..11s), i.e. a buffered frontier 1s PAST the
/// advertised window end, as live SABR servers really serve on join.
fn live_two_segments_response() -> Vec<u8> {
    let mut out = Vec::new();
    live_metadata_part(&mut out);
    emit_segment(
        &mut out,
        ITAG,
        LMT,
        1,
        100,
        false,
        1000,
        5000,
        b"LIVE-SEG100",
    );
    emit_segment(
        &mut out,
        ITAG,
        LMT,
        2,
        101,
        false,
        6000,
        5000,
        b"LIVE-SEG101",
    );
    out
}

/// A `LiveMetadata` part with a `[0, window_end_ms]` seekable window and the
/// head 10s past it, for DVR-shaped scenarios.
fn live_metadata_part_windowed(out: &mut Vec<u8>, window_end_ms: i64) {
    let lm = LiveMetadata {
        head_sequence_number: 1000,
        head_sequence_time_ms: window_end_ms + 10_000,
        min_seekable_time_ticks: 0,
        min_seekable_timescale: 1000,
        max_seekable_time_ticks: window_end_ms,
        max_seekable_timescale: 1000,
        ..Default::default()
    };
    ump_part(out, PartType::LiveMetadata, &lm.encode_to_vec());
}

/// The join response of the DVR-wedge scenario: a wide `[0s,200s]` window
/// (the requested position sits deep inside it) and segments 100/101.
fn dvr_two_segments_response() -> Vec<u8> {
    let mut out = Vec::new();
    live_metadata_part_windowed(&mut out, 200_000);
    emit_segment(
        &mut out,
        ITAG,
        LMT,
        1,
        100,
        false,
        1000,
        5000,
        b"LIVE-SEG100",
    );
    emit_segment(
        &mut out,
        ITAG,
        LMT,
        2,
        101,
        false,
        6000,
        5000,
        b"LIVE-SEG101",
    );
    out
}

/// The live-head sentinel a live join (or stall rejoin) request must carry,
/// in ms (JS `Number.MAX_SAFE_INTEGER`).
const LIVE_HEAD_SENTINEL_MS: i64 = 9_007_199_254_740_991;

fn player_time_ms(body: &[u8]) -> Option<i64> {
    VideoPlaybackAbrRequest::decode(body)
        .expect("decode request")
        .client_abr_state
        .expect("client abr state")
        .player_time_ms
}

async fn wait_until<F: Fn() -> bool>(timeout: Duration, cond: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    cond()
}

/// Spawn the session pump on the test runtime. Aborts it on drop.
fn spawn_pump(session: &SabrSession) -> tokio::task::JoinHandle<()> {
    let session = session.clone();
    tokio::spawn(async move { session.run().await })
}

#[tokio::test]
async fn pumps_a_vod_stream_into_buffers() {
    let (transport, requests) = SabrTransport::canned(vec![build_response()]);
    let session = SabrSession::new(spec(), transport);
    let video = video_format();
    let buffer = session.buffer_for(&video);

    session.set_demand(Role::Video, video.clone(), 0);
    let _pump = spawn_pump(&session);

    // The init segment and all three media segments should arrive and complete.
    assert!(
        wait_until(Duration::from_secs(3), || {
            buffer.init_segment().is_some()
                && buffer.get(2).map(|s| s.is_complete()).unwrap_or(false)
        })
        .await,
        "segments did not arrive"
    );

    let init = buffer.init_segment().expect("init segment");
    assert!(init.is_init);
    assert_eq!(init.to_vec(), b"INIT");

    let seg0 = buffer.get(0).expect("seg0");
    assert!(seg0.is_complete());
    assert_eq!(seg0.to_vec(), b"SEG0-data");
    assert_eq!(seg0.duration_us(), 1_000_000);

    let seg2 = buffer.get(2).expect("seg2");
    assert_eq!(seg2.to_vec(), b"SEG2-data");
    assert_eq!(buffer.last_completed_from_front(), 2);

    // Format initialization metadata should have been captured.
    let fim = session
        .format_initialization_for(&video)
        .expect("format init");
    assert_eq!(fim.end_segment_number, 2);

    // The first request body should be a well-formed VideoPlaybackAbrRequest
    // asking for our itag.
    let first_body = requests.lock()[0].clone();
    let req = VideoPlaybackAbrRequest::decode(first_body.as_slice()).expect("decode request");
    assert_eq!(req.video_playback_ustreamer_config, vec![1, 2, 3]);
    assert_eq!(req.preferred_video_format_ids.len(), 1);
    assert_eq!(req.preferred_video_format_ids[0].itag, ITAG);
    assert_eq!(
        req.client_abr_state.as_ref().unwrap().player_time_ms,
        Some(0)
    );

    session.release();
    assert!(session.is_released());
}

#[tokio::test]
async fn surfaces_http_403_as_blocked() {
    let session = SabrSession::new(spec(), SabrTransport::canned_status(403));
    let video = video_format();
    session.set_demand(Role::Video, video, 0);
    let _pump = spawn_pump(&session);

    assert!(
        wait_until(Duration::from_secs(3), || session.fatal_error().is_some()).await,
        "expected a fatal error"
    );
    let err = session.fatal_error().unwrap();
    assert!(err.contains("blocked") || err.contains("403"), "got: {err}");

    session.release();
}

#[tokio::test]
async fn pumps_an_audio_only_stream_into_buffers() {
    // Regression: with no video demand, `on_media_header` used to lock the
    // session state twice in one expression (the second `.or_else` branch),
    // which self-deadlocks the pump thread on the first non-init audio
    // MediaHeader. If that regresses, the segments never complete and this
    // times out.
    let (transport, requests) = SabrTransport::canned(vec![build_audio_response(true)]);
    let session = SabrSession::new(audio_spec(), transport);
    let audio = audio_format();
    let buffer = session.buffer_for(&audio);

    session.set_demand(Role::Audio, audio.clone(), 0);
    let _pump = spawn_pump(&session);

    assert!(
        wait_until(Duration::from_secs(3), || {
            buffer.init_segment().is_some()
                && buffer.get(2).map(|s| s.is_complete()).unwrap_or(false)
        })
        .await,
        "audio segments did not arrive (pump likely deadlocked)"
    );

    let init = buffer.init_segment().expect("audio init segment");
    assert!(init.is_init);
    assert_eq!(init.to_vec(), b"AINIT");

    let seg0 = buffer.get(0).expect("audio seg0");
    assert!(seg0.is_complete());
    assert_eq!(seg0.to_vec(), b"AUDIO-SEG0");
    assert_eq!(seg0.duration_us(), 1_000_000);
    assert_eq!(buffer.last_completed_from_front(), 2);

    // The request must ask for the audio itag only, with no video demanded and
    // the enabled-track bitfield set to audio.
    let first_body = requests.lock()[0].clone();
    let req = VideoPlaybackAbrRequest::decode(first_body.as_slice()).expect("decode request");
    assert!(req.preferred_video_format_ids.is_empty());
    assert_eq!(req.preferred_audio_format_ids.len(), 1);
    assert_eq!(req.preferred_audio_format_ids[0].itag, AUDIO_ITAG);
    assert_eq!(
        req.client_abr_state
            .as_ref()
            .unwrap()
            .enabled_track_types_bitfield,
        MediaType::Audio as i32
    );

    session.release();
    assert!(session.is_released());
}

#[tokio::test]
async fn audio_only_with_inexact_durations_does_not_deadlock() {
    // Same audio-only deadlock site, but exercised via the inexact-duration
    // branch (MediaHeaders carry no duration, so `on_media_header` runs
    // back-patching / estimation right around the double-lock). Segments must
    // still complete.
    let (transport, _requests) = SabrTransport::canned(vec![build_audio_response(false)]);
    let session = SabrSession::new(audio_spec(), transport);
    let audio = audio_format();
    let buffer = session.buffer_for(&audio);

    session.set_demand(Role::Audio, audio.clone(), 0);
    let _pump = spawn_pump(&session);

    assert!(
        wait_until(Duration::from_secs(3), || {
            buffer.init_segment().is_some()
                && buffer.get(2).map(|s| s.is_complete()).unwrap_or(false)
        })
        .await,
        "audio segments did not arrive (pump likely deadlocked)"
    );
    assert_eq!(buffer.get(0).expect("audio seg0").to_vec(), b"AUDIO-SEG0");

    session.release();
}

#[tokio::test]
async fn live_keepalive_seek_does_not_clear_buffer() {
    // Regression: a live server re-issues SABR_SEEK to ~the current position as a
    // keep-alive on nearly every request. Treating that as a real reposition
    // (clearing the buffer + restarting) starves live playback. The covering
    // segment is wiped, `seek_pending` never lands, and the request pins forever
    // while the server loops SABR_SEEK + empty headers. A keep-alive seek must be
    // a no-op. Here segment 100 (from response 1) must survive the keep-alive
    // seek in response 2, and segment 101 (parsed *after* the seek) must still be
    // processed rather than dropped by a mid-response epoch bump.
    let mut r1 = Vec::new();
    live_metadata_part(&mut r1);
    emit_segment(
        &mut r1,
        ITAG,
        LMT,
        1,
        100,
        false,
        1000,
        5000,
        b"LIVE-SEG100",
    );

    let mut r2 = Vec::new();
    live_metadata_part(&mut r2);
    // Seek to 1.0s, exactly segment 100's start, i.e. a keep-alive to where we
    // already are / already have buffered.
    sabr_seek_part(&mut r2, 1000, 1000);
    emit_segment(
        &mut r2,
        ITAG,
        LMT,
        2,
        101,
        false,
        6000,
        5000,
        b"LIVE-SEG101",
    );

    let (transport, _requests) = SabrTransport::canned(vec![r1, r2]);
    let session = SabrSession::new(live_spec(), transport);
    let video = video_format();
    let buffer = session.buffer_for(&video);

    session.set_demand(Role::Video, video.clone(), 0);
    let _pump = spawn_pump(&session);

    // Segment 101 arriving proves response 2 was consumed past the keep-alive
    // seek (a real reposition would have cleared/aborted it).
    assert!(
        wait_until(Duration::from_secs(3), || {
            buffer.get(101).map(|s| s.is_complete()).unwrap_or(false)
        })
        .await,
        "segment after keep-alive seek never arrived (seek treated as a reposition)"
    );
    // And the earlier segment must have survived the keep-alive seek.
    assert!(
        buffer.get(100).is_some(),
        "keep-alive seek wiped an already-buffered segment"
    );
    assert_eq!(buffer.get(100).unwrap().to_vec(), b"LIVE-SEG100");

    session.release();
}

#[tokio::test]
async fn live_request_position_is_clamped_to_the_window_end() {
    // Field failure (livestream-fail): the server serves readahead past its
    // advertised seekable window on join, and the feeders report the buffered
    // frontier as the playback position. Reporting that frontier as the player
    // time reads as a playhead past the live edge, and the server answers with
    // corrective seeks + empty media instead of the next segment. The reported
    // position must be clamped to the window end.
    let (transport, requests) = SabrTransport::canned(vec![live_two_segments_response()]);
    let session = SabrSession::new(live_spec(), transport);
    let video = video_format();

    session.set_demand(Role::Video, video.clone(), 0);
    // What the feeders would report after pushing both segments: the frontier.
    session.set_playback_position(11_000_000);
    let _pump = spawn_pump(&session);

    assert!(
        wait_until(Duration::from_secs(3), || requests.lock().len() >= 2).await,
        "second request never fired"
    );
    let bodies = requests.lock().clone();
    // The join request carries the live-head sentinel.
    assert_eq!(player_time_ms(&bodies[0]), Some(LIVE_HEAD_SENTINEL_MS));
    // The follow-up reports at most the window end (10s), not the 11s frontier.
    assert_eq!(player_time_ms(&bodies[1]), Some(10_000));

    session.release();
}

#[tokio::test]
async fn live_backward_seek_in_an_empty_response_is_acked() {
    // Field failure (livestream-fail): after serving ~10s of readahead the
    // server decided the reported position was past the allowed live edge and
    // answered every request with a corrective backward SABR_SEEK, zero media,
    // and a directed backoff, while the session re-sent the same rejected
    // position forever (playback froze at 10s once the buffer drained). The
    // correction must not flush buffers (the keep-alive rule), but it MUST be
    // acked in the next reported position so the server resumes serving.
    let mut correction = Vec::new();
    live_metadata_part(&mut correction);
    // Backward to 6s, the start of the newest buffered segment, mirroring the
    // field servers' constant correction target.
    sabr_seek_part(&mut correction, 6000, 1000);
    let policy = NextRequestPolicy {
        backoff_time_ms: 300,
        ..Default::default()
    };
    ump_part(
        &mut correction,
        PartType::NextRequestPolicy,
        &policy.encode_to_vec(),
    );

    let mut recovery = Vec::new();
    live_metadata_part(&mut recovery);
    emit_segment(
        &mut recovery,
        ITAG,
        LMT,
        1,
        102,
        false,
        11_000,
        5000,
        b"LIVE-SEG102",
    );

    let (transport, requests) =
        SabrTransport::canned(vec![live_two_segments_response(), correction, recovery]);
    let session = SabrSession::new(live_spec(), transport);
    let video = video_format();
    let buffer = session.buffer_for(&video);

    session.set_demand(Role::Video, video.clone(), 0);
    session.set_playback_position(11_000_000);
    let _pump = spawn_pump(&session);

    // Segment 102 arriving proves the post-correction response was reached,
    // i.e. the session did not livelock on the rejected position.
    assert!(
        wait_until(Duration::from_secs(5), || {
            buffer.get(102).map(|s| s.is_complete()).unwrap_or(false)
        })
        .await,
        "session never recovered after the corrective seek"
    );

    // The request after the correction reports the acked position.
    let bodies = requests.lock().clone();
    assert_eq!(player_time_ms(&bodies[2]), Some(6_000));

    // And the correction flushed nothing and signalled no discontinuity.
    assert!(buffer.get(100).is_some(), "correction wiped segment 100");
    assert!(buffer.get(101).is_some(), "correction wiped segment 101");
    assert_eq!(session.server_seek_generation(), 0);

    session.release();
}

#[tokio::test]
async fn live_keepalive_seek_with_media_is_not_acked() {
    // Field failure (a DVR live stream frozen at 5s): the server answers each
    // request with the segment CONTAINING the reported player time plus a
    // SABR_SEEK back to that segment's start. Acking that echo pins the
    // reported position at the segment start, so the server re-serves the
    // same segment forever. Only a backward seek in an EMPTY response (a
    // positional refusal) may be acked; one riding along with media must not
    // move the reported position.
    let mut echo = Vec::new();
    live_metadata_part(&mut echo);
    // The same segment 101 again (duplicate media bytes) plus a seek to its
    // start, the keep-alive echo shape.
    emit_segment(
        &mut echo,
        ITAG,
        LMT,
        1,
        101,
        false,
        6000,
        5000,
        b"LIVE-SEG101",
    );
    sabr_seek_part(&mut echo, 6000, 1000);

    let mut next = Vec::new();
    live_metadata_part(&mut next);
    emit_segment(
        &mut next,
        ITAG,
        LMT,
        1,
        102,
        false,
        11_000,
        5000,
        b"LIVE-SEG102",
    );

    let (transport, requests) =
        SabrTransport::canned(vec![live_two_segments_response(), echo, next]);
    let session = SabrSession::new(live_spec(), transport);
    let video = video_format();
    let buffer = session.buffer_for(&video);

    session.set_demand(Role::Video, video.clone(), 0);
    session.set_playback_position(11_000_000);
    let _pump = spawn_pump(&session);

    assert!(
        wait_until(Duration::from_secs(5), || {
            buffer.get(102).map(|s| s.is_complete()).unwrap_or(false)
        })
        .await,
        "the session never progressed past the keep-alive echo"
    );

    // The request after the echo must NOT report the seek target (6s): the
    // position stays at the clamped frontier, so the server serves the NEXT
    // segment instead of the same one again.
    let bodies = requests.lock().clone();
    assert_eq!(player_time_ms(&bodies[2]), Some(10_000));
    assert!(buffer.get(100).is_some());
    assert!(buffer.get(101).is_some());
    assert_eq!(session.server_seek_generation(), 0);

    session.release();
}

#[tokio::test]
async fn live_dvr_stall_rejoins_at_the_position() {
    // A viewer deep in the DVR window (window end 200s, playing at 11s) whose
    // stream stalls must be re-placed at THEIR position, not yanked to the
    // live head.
    let mut placement = Vec::new();
    live_metadata_part_windowed(&mut placement, 200_000);
    emit_segment(
        &mut placement,
        ITAG,
        LMT,
        1,
        102,
        false,
        11_000,
        5000,
        b"LIVE-SEG102",
    );

    let (transport, requests) = SabrTransport::canned(vec![
        dvr_two_segments_response(),
        Vec::new(),
        Vec::new(),
        placement,
    ]);
    let session = SabrSession::new(live_spec(), transport);
    session.set_live_stall_rejoin_ms(100);
    let video = video_format();
    let buffer = session.buffer_for(&video);

    session.set_demand(Role::Video, video.clone(), 0);
    session.set_playback_position(11_000_000);
    let _pump = spawn_pump(&session);

    assert!(
        wait_until(Duration::from_secs(6), || {
            buffer.get(102).map(|s| s.is_complete()).unwrap_or(false)
        })
        .await,
        "placement media never arrived after the DVR stall rejoin"
    );

    // The rejoin request re-asked for the DVR position, not the head sentinel.
    let bodies = requests.lock().clone();
    assert_eq!(player_time_ms(&bodies[3]), Some(11_000));
    // And it was a real rejoin: stale media dropped, consumers told to resync.
    assert!(
        buffer.get(100).is_none(),
        "stale segment survived the rejoin"
    );
    assert!(session.server_seek_generation() >= 1);

    session.release();
}

#[tokio::test]
async fn healthy_live_cadence_never_trips_the_stall_rejoin() {
    // Media arriving between empty polls must keep resetting the stall clock.
    // Without the reset-on-advance every healthy live stream would silently
    // flush its buffers and rejoin every LIVE_STALL_REJOIN_MS. The threshold
    // here is far below the 1s poll gap, so any run of TWO empty polls
    // without media in between would trip it; the healthy cadence has runs
    // of exactly one.
    let media = |seq: i32, start_ms: i64| {
        let mut out = Vec::new();
        live_metadata_part(&mut out);
        emit_segment(
            &mut out,
            ITAG,
            LMT,
            1,
            seq,
            false,
            start_ms,
            5000,
            b"LIVE-SEG",
        );
        out
    };
    // Two cycles only: segment 103 fills the 20s default readahead target
    // exactly, so the pump goes idle afterwards and no tail of default-empty
    // responses can trip the shortened threshold after the cadence ends.
    let (transport, requests) = SabrTransport::canned(vec![
        live_two_segments_response(),
        Vec::new(),
        media(102, 11_000),
        Vec::new(),
        media(103, 16_000),
    ]);
    let session = SabrSession::new(live_spec(), transport);
    session.set_live_stall_rejoin_ms(100);
    let video = video_format();
    let buffer = session.buffer_for(&video);

    session.set_demand(Role::Video, video.clone(), 0);
    session.set_playback_position(11_000_000);
    let _pump = spawn_pump(&session);

    assert!(
        wait_until(Duration::from_secs(10), || {
            buffer.get(103).map(|s| s.is_complete()).unwrap_or(false)
        })
        .await,
        "the alternating cadence never played out"
    );

    let generation = session.server_seek_generation();
    let sentinel_sent = requests
        .lock()
        .iter()
        .skip(1)
        .any(|b| player_time_ms(b) == Some(LIVE_HEAD_SENTINEL_MS));
    let kept_media = buffer.get(100).is_some();
    session.release();

    assert_eq!(generation, 0, "a healthy cadence tripped the stall rejoin");
    assert!(
        !sentinel_sent,
        "a healthy cadence re-sent the join sentinel"
    );
    assert!(kept_media, "a healthy cadence flushed buffered media");
}

#[tokio::test]
async fn live_dvr_rejoins_escalate_to_the_head() {
    // A server that stays dry through repeated DVR-position rejoins gets the
    // recovery of last resort: a live-head rejoin with the sentinel.
    let (transport, requests) = SabrTransport::canned(vec![dvr_two_segments_response()]);
    let session = SabrSession::new(live_spec(), transport);
    session.set_live_stall_rejoin_ms(100);
    let video = video_format();

    session.set_demand(Role::Video, video.clone(), 0);
    session.set_playback_position(11_000_000);
    let _pump = spawn_pump(&session);

    assert!(
        wait_until(Duration::from_secs(15), || {
            requests
                .lock()
                .iter()
                .skip(1)
                .any(|b| player_time_ms(b) == Some(LIVE_HEAD_SENTINEL_MS))
        })
        .await,
        "the stall never escalated to a live-head rejoin"
    );
    // The DVR-position attempts came first.
    let bodies = requests.lock().clone();
    assert!(
        bodies.iter().any(|b| player_time_ms(b) == Some(11_000)),
        "no DVR-position rejoin before the head escalation"
    );

    session.release();
}

#[tokio::test]
async fn live_stall_rejoins_at_the_head() {
    // Backstop for any server refusal mode: a live session that gets no media
    // for the stall threshold abandons its position, clears its buffers, bumps
    // the server-seek generation (so feeders resync from the new front), and
    // re-requests the live-head sentinel exactly like a fresh join.
    let mut placement = Vec::new();
    live_metadata_part(&mut placement);
    emit_segment(
        &mut placement,
        ITAG,
        LMT,
        1,
        200,
        false,
        20_000,
        5000,
        b"LIVE-SEG200",
    );

    // Two empty responses: the first starts the stall clock, the second (one
    // live-poll later) trips the threshold and triggers the rejoin.
    let (transport, requests) = SabrTransport::canned(vec![
        live_two_segments_response(),
        Vec::new(),
        Vec::new(),
        placement,
    ]);
    let session = SabrSession::new(live_spec(), transport);
    session.set_live_stall_rejoin_ms(100);
    let video = video_format();
    let buffer = session.buffer_for(&video);

    session.set_demand(Role::Video, video.clone(), 0);
    session.set_playback_position(11_000_000);
    let _pump = spawn_pump(&session);

    assert!(
        wait_until(Duration::from_secs(6), || {
            buffer.get(200).map(|s| s.is_complete()).unwrap_or(false)
        })
        .await,
        "placement media never arrived after the stall rejoin"
    );

    // The rejoin request re-used the live-head sentinel.
    let bodies = requests.lock().clone();
    assert_eq!(player_time_ms(&bodies[3]), Some(LIVE_HEAD_SENTINEL_MS));
    // Stale pre-stall media was dropped and consumers were told to resync.
    assert!(
        buffer.get(100).is_none(),
        "stale segment survived the rejoin"
    );
    assert!(session.server_seek_generation() >= 1);

    session.release();
}

#[tokio::test]
async fn honours_a_server_backoff_before_the_next_request() {
    // Field failure: the server answered the first request with no media and a
    // directed backoff; the wait must be visible (feeders extend their init
    // deadline by it, the GUI shows a countdown), the next request must not
    // fire early, and the directed wait must never be treated as fatal.
    let mut backoff_only = Vec::new();
    let policy = NextRequestPolicy {
        backoff_time_ms: 700,
        ..Default::default()
    };
    ump_part(
        &mut backoff_only,
        PartType::NextRequestPolicy,
        &policy.encode_to_vec(),
    );

    let (transport, requests) = SabrTransport::canned(vec![backoff_only, build_response()]);
    let session = SabrSession::new(spec(), transport);
    let video = video_format();
    let buffer = session.buffer_for(&video);

    // 0 marks BackoffEnded.
    let events: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let events = events.clone();
        session.set_listener(Some(Arc::new(move |event| match event {
            SabrSessionEvent::Backoff { delay_ms } => events.lock().push(delay_ms),
            SabrSessionEvent::BackoffEnded => events.lock().push(0),
            _ => {}
        })));
    }

    session.set_demand(Role::Video, video.clone(), 0);
    let started = Instant::now();
    let _pump = spawn_pump(&session);

    assert!(
        wait_until(Duration::from_secs(3), || session.backoff_remaining_ms()
            > 0)
        .await,
        "the directed backoff never became visible"
    );
    assert_eq!(requests.lock().len(), 1, "request fired during the backoff");
    assert!(session.fatal_error().is_none());

    assert!(
        wait_until(Duration::from_secs(5), || {
            buffer.get(2).map(|s| s.is_complete()).unwrap_or(false)
        })
        .await,
        "media never arrived after the backoff"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(700),
        "the second request did not wait out the backoff"
    );
    assert_eq!(requests.lock().len(), 2);
    assert!(session.fatal_error().is_none());

    let seen = events.lock().clone();
    assert!(
        seen.first().is_some_and(|d| *d > 0),
        "no Backoff event, got: {seen:?}"
    );
    assert!(seen.contains(&0), "no BackoffEnded event, got: {seen:?}");

    session.release();
}

// --- regression fixtures: client restarts racing server directives ---

/// A response carrying `count` consecutive `dur_ms` segments for `itag` from
/// `start_ms`, behind an init segment and a FormatInitializationMetadata that
/// declares no end (so nothing ever reads as complete).
fn long_response(itag: i32, lmt: u64, count: i32, dur_ms: i64) -> Vec<u8> {
    let mut out = Vec::new();
    let init = FormatInitializationMetadata {
        video_id: "vid".into(),
        format_id: Some(FormatId {
            itag,
            lmt,
            xtags: String::new(),
        }),
        mime_type: "video/mp4".into(),
        ..Default::default()
    };
    ump_part(
        &mut out,
        PartType::FormatInitializationMetadata,
        &init.encode_to_vec(),
    );
    emit_segment(&mut out, itag, lmt, 1, 0, true, 0, 0, b"INIT");
    for seq in 0..count {
        emit_segment(
            &mut out,
            itag,
            lmt,
            2 + seq,
            seq,
            false,
            seq as i64 * dur_ms,
            dur_ms,
            b"SEG",
        );
    }
    out
}

/// One media segment at `start_ms` for the default video format, behind its
/// init and format metadata.
fn segment_at(seq: i32, start_ms: i64) -> Vec<u8> {
    let mut out = Vec::new();
    let init = FormatInitializationMetadata {
        video_id: "vid".into(),
        format_id: Some(FormatId {
            itag: ITAG,
            lmt: LMT,
            xtags: String::new(),
        }),
        mime_type: "video/mp4".into(),
        ..Default::default()
    };
    ump_part(
        &mut out,
        PartType::FormatInitializationMetadata,
        &init.encode_to_vec(),
    );
    emit_segment(&mut out, ITAG, LMT, 1, 0, true, 0, 0, b"INIT");
    emit_segment(&mut out, ITAG, LMT, 2, seq, false, start_ms, 1000, b"SEG");
    out
}

fn format_init_only() -> Vec<u8> {
    let mut out = Vec::new();
    let init = FormatInitializationMetadata {
        video_id: "vid".into(),
        format_id: Some(FormatId {
            itag: ITAG,
            lmt: LMT,
            xtags: String::new(),
        }),
        ..Default::default()
    };
    ump_part(
        &mut out,
        PartType::FormatInitializationMetadata,
        &init.encode_to_vec(),
    );
    out
}

/// A spec long enough that no frontier check reads the stream as finished.
fn long_spec() -> SabrStreamSpec {
    SabrStreamSpec {
        duration_us: 600_000_000,
        ..spec()
    }
}

/// Install a listener that calls `session.restart(to_us)` once, from inside
/// the pump, the first time a FormatInitializationMetadata part is consumed.
/// That is the one hook that lets a test land a client restart at a
/// deterministic point INSIDE a response.
fn restart_on_first_format_init(session: &SabrSession, to_us: i64) {
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let target = session.clone();
    session.set_listener(Some(Arc::new(move |event| {
        if let SabrSessionEvent::FormatInitialization(_) = event
            && !fired.swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            target.restart(to_us, true);
        }
    })));
}

#[tokio::test]
async fn a_client_restart_cancels_a_pending_server_seek() {
    // A genuine SABR_SEEK parks `seek_pending` until a covering segment
    // lands. A client seek that arrives before then is the newer intent, and
    // once its own resume request has been answered the pending server seek
    // must not take the request position back to the server's target.
    let mut server_seek = Vec::new();
    sabr_seek_part(&mut server_seek, 3_000_000, 1000);

    let (transport, requests) = SabrTransport::canned(vec![
        server_seek,
        format_init_only(),
        segment_at(200, 1_000_000),
        Vec::new(),
    ]);
    let session = SabrSession::new(long_spec(), transport);
    let video = video_format();
    // Fires while response 2 is consumed, after the server seek is pending.
    restart_on_first_format_init(&session, 1_000_000_000);

    session.set_demand(Role::Video, video.clone(), 0);
    let _pump = spawn_pump(&session);

    assert!(
        wait_until(Duration::from_secs(5), || requests.lock().len() >= 4).await,
        "the session never reached its fourth request"
    );
    let bodies = requests.lock().clone();
    assert_eq!(
        player_time_ms(&bodies[1]),
        Some(3_000_000),
        "server seek honoured"
    );
    assert_eq!(
        player_time_ms(&bodies[2]),
        Some(1_000_000),
        "client restart honoured"
    );
    assert_eq!(
        player_time_ms(&bodies[3]),
        Some(1_001_000),
        "the request after the restart's media landed went back to the stale server seek"
    );

    session.release();
}

#[tokio::test]
async fn a_server_seek_parsed_before_a_restart_is_not_applied_after_it() {
    // The seek directive is parsed early in a response and applied when the
    // response ends. A client restart landing in between makes it stale: it
    // was the server's answer to a position the client has since abandoned.
    // Applying it anyway re-anchors the demands and the request position to
    // the old target.
    let mut r1 = Vec::new();
    sabr_seek_part(&mut r1, 3_000_000, 1000);
    // Parsed after the seek; the listener restarts the session here.
    let fmt = format_init_only();
    r1.extend_from_slice(&fmt);

    let (transport, requests) =
        SabrTransport::canned(vec![r1, segment_at(200, 1_000_000), Vec::new()]);
    let session = SabrSession::new(long_spec(), transport);
    let video = video_format();
    restart_on_first_format_init(&session, 1_000_000_000);

    session.set_demand(Role::Video, video.clone(), 0);
    let _pump = spawn_pump(&session);

    assert!(
        wait_until(Duration::from_secs(5), || requests.lock().len() >= 3).await,
        "the session never reached its third request"
    );
    let bodies = requests.lock().clone();
    assert_eq!(
        player_time_ms(&bodies[1]),
        Some(1_000_000),
        "client restart honoured"
    );
    assert_eq!(
        player_time_ms(&bodies[2]),
        Some(1_001_000),
        "a stale server seek hijacked the position after the client restart"
    );
    assert_eq!(
        session.server_seek_generation(),
        0,
        "a stale server seek was reported to consumers as a discontinuity"
    );

    session.release();
}

#[tokio::test]
async fn a_redirect_parsed_before_a_restart_keeps_the_restart_position() {
    // A redirect re-issues the request it interrupted, on the new host. When a
    // client restart landed mid-response that re-issue must target the
    // restart's position, not the position the redirected request carried.
    let mut r1 = Vec::new();
    let redirect = sabrump::proto::SabrRedirect {
        url: "https://elsewhere.test/videoplayback".into(),
    };
    ump_part(&mut r1, PartType::SabrRedirect, &redirect.encode_to_vec());
    let fmt = format_init_only();
    r1.extend_from_slice(&fmt);

    let (transport, requests) = SabrTransport::canned(vec![r1, Vec::new()]);
    let session = SabrSession::new(long_spec(), transport);
    let video = video_format();
    restart_on_first_format_init(&session, 1_000_000_000);

    session.set_demand(Role::Video, video.clone(), 0);
    let _pump = spawn_pump(&session);

    assert!(
        wait_until(Duration::from_secs(5), || requests.lock().len() >= 2).await,
        "the redirected request never fired"
    );
    let bodies = requests.lock().clone();
    assert_eq!(player_time_ms(&bodies[0]), Some(0));
    assert_eq!(
        player_time_ms(&bodies[1]),
        Some(1_000_000),
        "the redirect re-issued the pre-restart position instead of the restart's"
    );

    session.release();
}

// --- regression fixtures: eviction ---

#[tokio::test]
async fn eviction_keeps_up_with_playback_after_a_restart() {
    // Every playback starts with a restart (appsrc's seek-data at 0) and every
    // seek is one. The eviction floor must follow the playhead afterwards, or
    // everything fetched since the last seek is held for the rest of the item.
    let (transport, requests) =
        SabrTransport::canned(vec![long_response(ITAG, LMT, 10, 10_000), Vec::new()]);
    let session = SabrSession::new(long_spec(), transport);
    let video = video_format();
    let buffer = session.buffer_for(&video);

    session.set_demand(Role::Video, video.clone(), 0);
    session.restart(0, true);
    let _pump = spawn_pump(&session);

    assert!(
        wait_until(Duration::from_secs(3), || {
            buffer.get(9).map(|s| s.is_complete()).unwrap_or(false)
        })
        .await,
        "segments did not arrive"
    );
    // The consumer has pushed up to 90s. The next request evicts first.
    session.set_playback_position(90_000_000);
    session.advance_demand(Role::Video, 90_000_000);
    assert!(
        wait_until(Duration::from_secs(3), || requests.lock().len() >= 2).await,
        "the follow-up request never fired"
    );

    assert!(
        buffer.get(0).is_none(),
        "segment 0 (0s-10s) survived with the playhead at 90s and 30s keep-behind"
    );
    assert!(buffer.get(4).is_none(), "segment 4 (40s-50s) survived");
    assert!(
        buffer.get(6).is_some(),
        "segment 6 (60s-70s) was evicted inside keep-behind"
    );
    assert!(buffer.get(9).is_some());

    session.release();
}

#[tokio::test]
async fn eviction_never_passes_the_slowest_consumer() {
    // Video and audio feeders each report their own frontier as the playback
    // position, last writer wins. An audio feeder well ahead of the video
    // feeder must not let eviction remove video the video feeder has yet to
    // push: the feeder then waits forever for a sequence that is gone, while
    // the pump believes the run from the first remaining segment is enough.
    let mut r1 = long_response(ITAG, LMT, 10, 10_000);
    let audio = build_audio_long(10, 10_000);
    r1.extend_from_slice(&audio);

    let (transport, requests) = SabrTransport::canned(vec![r1, Vec::new()]);
    let session = SabrSession::new(
        SabrStreamSpec {
            audio_formats: vec![audio_format()],
            ..long_spec()
        },
        transport,
    );
    let video = video_format();
    let audio_fmt = audio_format();
    let video_buffer = session.buffer_for(&video);
    let audio_buffer = session.buffer_for(&audio_fmt);

    session.set_demand(Role::Video, video.clone(), 0);
    session.set_demand(Role::Audio, audio_fmt.clone(), 0);
    let _pump = spawn_pump(&session);

    assert!(
        wait_until(Duration::from_secs(3), || {
            video_buffer
                .get(9)
                .map(|s| s.is_complete())
                .unwrap_or(false)
                && audio_buffer
                    .get(9)
                    .map(|s| s.is_complete())
                    .unwrap_or(false)
        })
        .await,
        "segments did not arrive"
    );
    // Audio pushed through 95s, video only through 5s.
    session.advance_demand(Role::Video, 5_000_000);
    session.set_playback_position(95_000_000);
    session.advance_demand(Role::Audio, 95_000_000);
    assert!(
        wait_until(Duration::from_secs(3), || requests.lock().len() >= 2).await,
        "the follow-up request never fired"
    );

    assert!(
        video_buffer.get(1).is_some(),
        "video segment 1 (10s-20s) was evicted while the video consumer was at 5s"
    );
    assert!(video_buffer.get(5).is_some());

    session.release();
}

/// `count` audio segments of `dur_ms` with the audio format's init, appended
/// to a response.
fn build_audio_long(count: i32, dur_ms: i64) -> Vec<u8> {
    let mut out = Vec::new();
    let init = FormatInitializationMetadata {
        video_id: "vid".into(),
        format_id: Some(FormatId {
            itag: AUDIO_ITAG,
            lmt: AUDIO_LMT,
            xtags: String::new(),
        }),
        mime_type: "audio/mp4".into(),
        ..Default::default()
    };
    ump_part(
        &mut out,
        PartType::FormatInitializationMetadata,
        &init.encode_to_vec(),
    );
    // Header ids continue past the video response's so they never collide.
    emit_segment(
        &mut out, AUDIO_ITAG, AUDIO_LMT, 100, 0, true, 0, 0, b"AINIT",
    );
    for seq in 0..count {
        emit_segment(
            &mut out,
            AUDIO_ITAG,
            AUDIO_LMT,
            101 + seq,
            seq,
            false,
            seq as i64 * dur_ms,
            dur_ms,
            b"ASEG",
        );
    }
    out
}

// --- regression fixtures: server-side format switch ---

#[tokio::test]
async fn a_server_format_switch_is_not_an_empty_response() {
    // With alternates offered, the server may answer with another rung. The
    // session adopts it, but the response is judged against the rung that was
    // active when the request went out, so media for the adopted rung reads
    // as "no media" and earns an empty-response backoff on every switch.
    let alternate = SabrFormat {
        itag: 136,
        last_modified: LMT + 1,
        height: 720,
        ..video_format()
    };
    let mut r1 = Vec::new();
    let init = FormatInitializationMetadata {
        video_id: "vid".into(),
        format_id: Some(FormatId {
            itag: alternate.itag,
            lmt: alternate.last_modified,
            xtags: String::new(),
        }),
        mime_type: "video/mp4".into(),
        ..Default::default()
    };
    ump_part(
        &mut r1,
        PartType::FormatInitializationMetadata,
        &init.encode_to_vec(),
    );
    emit_segment(&mut r1, 136, LMT + 1, 1, 0, true, 0, 0, b"INIT");
    emit_segment(&mut r1, 136, LMT + 1, 2, 0, false, 0, 1000, b"B0");
    emit_segment(&mut r1, 136, LMT + 1, 3, 1, false, 1000, 1000, b"B1");
    emit_segment(&mut r1, 136, LMT + 1, 4, 2, false, 2000, 1000, b"B2");

    let (transport, requests) = SabrTransport::canned(vec![r1, Vec::new()]);
    let session = SabrSession::new(long_spec(), transport);
    let preferred = video_format();
    let buffer = session.buffer_for(&alternate);

    session.set_demand_alternates(Role::Video, vec![preferred, alternate.clone()], 0);
    let _pump = spawn_pump(&session);

    assert!(
        wait_until(Duration::from_secs(3), || {
            buffer.get(2).map(|s| s.is_complete()).unwrap_or(false)
        })
        .await,
        "the adopted rung's segments did not arrive"
    );
    assert_eq!(
        session.active_format_key(Role::Video),
        Some(alternate.key()),
        "the server's rung was not adopted"
    );
    // Three seconds buffered against a 20s readahead: the next request is due
    // at once. An empty-response backoff would hold it for 500ms.
    assert!(
        wait_until(Duration::from_millis(250), || requests.lock().len() >= 2).await,
        "the request after a server format switch was held back by an empty-response backoff"
    );

    session.release();
}

// --- regression fixtures: listener re-entrancy ---

#[test]
fn a_listener_may_release_the_session() {
    // The obvious reaction to a SessionError is to release the session. The
    // listener runs on the pump; releasing from there must not deadlock it.
    // Own thread and runtime so a deadlocked pump cannot hang the test's own
    // polling, and is left behind rather than joined if it does.
    let session = SabrSession::new(spec(), SabrTransport::canned_status(500));
    let video = video_format();
    session.set_demand(Role::Video, video, 0);
    {
        let target = session.clone();
        session.set_listener(Some(Arc::new(move |event| {
            if let SabrSessionEvent::SessionError(_) = event {
                target.release();
            }
        })));
    }
    let pump = session.clone();
    let (done, returned) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(pump.run());
        let _ = done.send(());
    });

    // `is_released()` flips before `release()` re-takes the listener lock, so
    // only the pump RETURNING proves it did not deadlock behind its own lock.
    assert!(
        returned.recv_timeout(Duration::from_secs(3)).is_ok(),
        "the pump deadlocked on its listener lock when the listener released the session"
    );
    assert!(session.is_released());
}
