//! End-to-end pump test driven by a canned transport that replays UMP bytes.

use std::time::{Duration, Instant};

use prost::Message;
use sabrump::proto::{
    ByteRange, FormatId, FormatInitializationMetadata, LiveMetadata, MediaHeader, MediaType,
    NextRequestPolicy, SabrSeek, VideoPlaybackAbrRequest,
};
use sabrump::spec::Role;
use sabrump::{PartType, SabrFormat, SabrSession, SabrStreamSpec, SabrTransport};

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
    emit_segment(&mut out, AUDIO_ITAG, AUDIO_LMT, 2, 0, false, 0, dur, b"AUDIO-SEG0");
    emit_segment(&mut out, AUDIO_ITAG, AUDIO_LMT, 3, 1, false, 1000, dur, b"AUDIO-SEG1");
    emit_segment(&mut out, AUDIO_ITAG, AUDIO_LMT, 4, 2, false, 2000, dur, b"AUDIO-SEG2");

    let policy = NextRequestPolicy {
        target_audio_readahead_ms: 10_000,
        playback_cookie: b"cookie".to_vec(),
        ..Default::default()
    };
    ump_part(&mut out, PartType::NextRequestPolicy, &policy.encode_to_vec());

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
        req.client_abr_state.as_ref().unwrap().enabled_track_types_bitfield,
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
    emit_segment(&mut r1, ITAG, LMT, 1, 100, false, 1000, 5000, b"LIVE-SEG100");

    let mut r2 = Vec::new();
    live_metadata_part(&mut r2);
    // Seek to 1.0s, exactly segment 100's start, i.e. a keep-alive to where we
    // already are / already have buffered.
    sabr_seek_part(&mut r2, 1000, 1000);
    emit_segment(&mut r2, ITAG, LMT, 2, 101, false, 6000, 5000, b"LIVE-SEG101");

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
