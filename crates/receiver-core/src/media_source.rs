use std::{collections::HashMap, sync::Arc};

use anyhow::{Context, Result};
use bytes::Bytes;
use gst::prelude::*;
use parking_lot::Mutex;
use tracing::warn;

use crate::user_agent;

/// Bytes handed downstream per `need-data` pull.
const BYTES_CHUNK: u64 = 256 * 1024;

/// Apply request headers + a browser user-agent to an `fcasthttpsrc`.
pub fn configure_http_source(elem: &gst::Element, headers: Option<&HashMap<String, String>>) {
    let mut did_set_user_agent = false;
    if let Some(headers) = headers {
        let mut extra = gst::Structure::builder("reqwesthttpsrc-extra-headers");
        for (k, v) in headers {
            if k.eq_ignore_ascii_case("user-agent") {
                elem.set_property("user-agent", v);
                did_set_user_agent = true;
            } else {
                extra = extra.field(k, v);
            }
        }
        elem.set_property("extra-headers", extra.build());
    }
    if !did_set_user_agent {
        elem.set_property("user-agent", user_agent::random_browser_user_agent(None));
    }
}

/// Build a urisourcebin for an HTTP/file/DASH/HLS/`data:` URI, applying
/// `headers` per-load to THIS urisourcebin's `fcasthttpsrc` (no global side
/// channel). It parses its streams, so its src pads feed decodebin3 directly.
pub fn build_uri_source(
    uri: &str,
    headers: Option<HashMap<String, String>>,
) -> Result<gst::Element> {
    build_uri_source_with_head(uri, headers, None)
}

/// A prefetched head of the resource, injected into the per-load source
/// element so playback starts from memory and only the remainder streams.
pub struct PreloadedHead {
    pub bytes: Bytes,
    /// Total resource size; the http source needs it up front, fcomp does not.
    pub total: Option<u64>,
}

/// `build_uri_source` plus an optional prefetched head for the source element
/// urisourcebin creates (fcasthttpsrc or fcompsrc).
pub fn build_uri_source_with_head(
    uri: &str,
    headers: Option<HashMap<String, String>>,
    head: Option<PreloadedHead>,
) -> Result<gst::Element> {
    let usb = gst::ElementFactory::make("urisourcebin")
        .property("uri", uri)
        .property("parse-streams", true)
        .property("use-buffering", true)
        .build()
        .context("creating urisourcebin")?;
    if let Some(bin) = usb.downcast_ref::<gst::Bin>() {
        bin.connect_deep_element_added(move |_, _, elem| {
            match elem.factory().map(|f| f.name()).as_deref() {
                Some("fcasthttpsrc") => {
                    configure_http_source(elem, headers.as_ref());
                    // http needs the total up front; without it the head is unusable.
                    if let Some(head) = head.as_ref()
                        && let Some(total) = head.total
                    {
                        elem.set_property(
                            "preloaded-head",
                            gst::glib::Bytes::from_owned(head.bytes.clone()),
                        );
                        elem.set_property("preloaded-size", total);
                    }
                }
                Some("fcompsrc") => {
                    if let Some(head) = head.as_ref() {
                        elem.set_property(
                            "preloaded-head",
                            gst::glib::Bytes::from_owned(head.bytes.clone()),
                        );
                    }
                }
                _ => {}
            }
        });
    }
    Ok(usb)
}

/// Build a source that serves an already-prefetched item from memory: a
/// seekable `appsrc`, parsebin-wrapped so its pads carry PARSED streams
/// exactly as urisourcebin's `parse-streams=true` would produce.
pub fn build_bytes_source(bytes: Bytes) -> Result<gst::Element> {
    let appsrc = build_bytes_appsrc(bytes);
    wrap_with_parsebin(appsrc.upcast(), "fcast-bytes-source")
}

/// The seekable BYTES `appsrc` over `bytes`, sized to the buffer so downstream
/// duration-in-bytes and seek queries behave like a file.
fn build_bytes_appsrc(bytes: Bytes) -> gst_app::AppSrc {
    let len = bytes.len() as u64;
    let offset = Arc::new(Mutex::new(0u64));

    let appsrc = gst_app::AppSrc::builder()
        .format(gst::Format::Bytes)
        // Seekable, not RandomAccess: RandomAccess advertises pull scheduling,
        // which collides with the push-based feeding here.
        .stream_type(gst_app::AppStreamType::Seekable)
        .size(len as i64)
        .build();

    appsrc.set_callbacks(
        gst_app::AppSrcCallbacks::builder()
            .need_data({
                let bytes = bytes.clone();
                let offset = offset.clone();
                move |appsrc, _hint| {
                    let mut pos = offset.lock();
                    if *pos >= len {
                        let _ = appsrc.end_of_stream();
                        return;
                    }
                    let start = *pos;
                    let end = (start + BYTES_CHUNK).min(len);
                    // Zero-copy: slice bumps the refcount, from_slice takes ownership.
                    let chunk = bytes.slice(start as usize..end as usize);
                    let mut buffer = gst::Buffer::from_slice(chunk);
                    {
                        let buffer = buffer.get_mut().unwrap();
                        buffer.set_offset(start);
                        buffer.set_offset_end(end);
                    }
                    *pos = end;
                    // MUST release the lock before pushing: a re-entrant
                    // need-data from downstream would otherwise deadlock.
                    drop(pos);
                    match appsrc.push_buffer(buffer) {
                        Ok(_) => {}
                        Err(gst::FlowError::Flushing | gst::FlowError::Eos) => {}
                        Err(err) => warn!(?err, "fcast bytes source push failed"),
                    }
                }
            })
            .seek_data({
                let offset = offset.clone();
                move |_appsrc, new_offset| {
                    // Any offset is accepted: an out-of-range one just makes
                    // the next need-data emit EOS.
                    *offset.lock() = new_offset;
                    true
                }
            })
            .build(),
    );

    appsrc
}

/// Build the WHEP source directly. `fcastwhepsrcbin` is a URIHandler keyed on
/// the `fcastwhep://` scheme. It emits RTP, so it is parsebin-wrapped.
pub fn build_whep_source(http_url: &str) -> Result<gst::Element> {
    let src = gst::ElementFactory::make("fcastwhepsrcbin")
        .build()
        .context("creating fcastwhepsrcbin")?;
    let whep_uri = http_url.replacen("http://", "fcastwhep://", 1);
    src.dynamic_cast_ref::<gst::URIHandler>()
        .context("fcastwhepsrcbin is not a URIHandler")?
        .set_uri(&whep_uri)
        .context("setting the WHEP endpoint")?;
    wrap_with_parsebin(src, "fcast-whep-source")
}

/// Build the fwebrtc source directly: its signalling channel is a live object
/// that cannot travel through a URI. Emits RTP, so it is parsebin-wrapped.
pub fn build_fwebrtc_source<C: Into<gst::glib::Value>>(channel: C) -> Result<gst::Element> {
    let src = gst::ElementFactory::make("fwebrtcsrc")
        .build()
        .context("creating fwebrtcsrc")?;
    src.set_property_from_value("signalling-channel", &channel.into());
    wrap_with_parsebin(src, "fcast-fwebrtc-source")
}

/// Build the AirPlay mirror source directly. `airplaysrc` emits encoded
/// H.264/AAC, so decodebin3 takes it without a parsebin wrap.
#[cfg(feature = "airplay")]
pub fn build_airplay_mirror_source(mirror_uri: &str) -> Result<gst::Element> {
    let src = gst::ElementFactory::make("airplaysrc")
        .build()
        .context("creating airplaysrc")?;
    src.dynamic_cast_ref::<gst::URIHandler>()
        .context("airplaysrc is not a URIHandler")?
        .set_uri(mirror_uri)
        .context("setting the AirPlay mirror URI")?;
    Ok(src)
}

/// Wrap a source that emits RTP (or otherwise unparsed) streams so its output
/// pads carry PARSED streams, mirroring urisourcebin's `parse-streams`.
///
/// `name` is a PREFIX: the bin gets a unique suffix, because two of these can
/// share one pipeline and GStreamer rejects duplicate child names.
fn wrap_with_parsebin(source: gst::Element, name: &str) -> Result<gst::Element> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let bin = gst::Bin::builder().name(format!("{name}-{seq}")).build();
    bin.add(&source).context("adding source to the parse bin")?;
    source.connect_pad_added({
        let bin = bin.downgrade();
        move |_, pad| {
            let Some(bin) = bin.upgrade() else { return };
            if let Err(err) = attach_parsebin(&bin, pad) {
                warn!(?err, pad = %pad.name(), "failed to attach parsebin to source pad");
            }
        }
    });
    // Any pads the source already exposes statically.
    for pad in source.src_pads() {
        if let Err(err) = attach_parsebin(&bin, &pad) {
            warn!(?err, "failed to attach parsebin to an existing source pad");
        }
    }
    Ok(bin.upcast())
}

/// Add a `parsebin` for one source pad and ghost its parsed output out of
/// `bin`.
fn attach_parsebin(bin: &gst::Bin, srcpad: &gst::Pad) -> Result<()> {
    let parsebin = gst::ElementFactory::make("parsebin")
        .build()
        .context("creating parsebin")?;
    bin.add(&parsebin)
        .context("adding parsebin to the parse bin")?;
    parsebin.connect_pad_added({
        let bin = bin.downgrade();
        move |_, pad| {
            let Some(bin) = bin.upgrade() else { return };
            match gst::GhostPad::with_target(pad) {
                Ok(ghost) => {
                    let _ = ghost.set_active(true);
                    if let Err(err) = bin.add_pad(&ghost) {
                        warn!(?err, "failed to ghost a parsed pad");
                    }
                }
                Err(err) => warn!(?err, "failed to create a ghost pad for a parsed stream"),
            }
        }
    });
    parsebin
        .sync_state_with_parent()
        .context("syncing parsebin")?;
    let sink = parsebin
        .static_pad("sink")
        .context("parsebin has no sink pad")?;
    srcpad
        .link(&sink)
        .context("linking the source pad into parsebin")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Once;

    use bytes::Bytes;
    use gst::prelude::*;

    fn init() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            gst::init().unwrap();
            // decodebin3 autoplugs fimagedec for the GIF test.
            crate::imagedec::plugin_init().unwrap();
        });
    }

    /// `seconds` of silence encoded to CBR MP3 (audio/mpeg, the fcomp
    /// container), for a fake companion provider to serve.
    fn make_mp3_bytes(seconds: f64) -> Bytes {
        make_mp3_bytes_at(seconds, 128)
    }

    /// Synthesized in-process rather than encoded through
    /// audiotestsrc ! lamemp3enc: the static gstreamer-full deliberately culls
    /// both (test/encode elements, see xtask/src/gstreamer.rs DISABLE_COMMON),
    /// and this suite only ever runs against that build.
    ///
    /// An MPEG-1 Layer III frame whose side info is all zeros carries no main
    /// data (every part2_3_length is 0) and decodes as silence, so a valid
    /// header plus zeros is a complete CBR frame. 1152 samples per frame.
    fn make_mp3_bytes_at(seconds: f64, bitrate_kbps: i32) -> Bytes {
        const RATE: f64 = 44100.0;
        const BITRATES: [i32; 14] = [
            32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
        ];
        let bitrate_index = BITRATES
            .iter()
            .position(|&b| b == bitrate_kbps)
            .expect("a valid MPEG-1 Layer III bitrate") as u8
            + 1;
        let frame_len = (144_000 * bitrate_kbps as usize) / RATE as usize;
        let frames = (seconds * RATE / 1152.0).round() as usize;
        let mut bytes = vec![0u8; frames * frame_len];
        for frame in bytes.chunks_exact_mut(frame_len) {
            // Sync, MPEG-1, Layer III, no CRC; 44100 Hz; stereo, original.
            frame[0] = 0xFF;
            frame[1] = 0xFB;
            frame[2] = bitrate_index << 4;
            frame[3] = 0x04;
        }
        Bytes::from(bytes)
    }

    /// Register a fake companion provider on `ctx` serving `(resource_id,
    /// bytes, reported_size)`; `reported_size` > `bytes.len()` fakes a
    /// byte-size overshoot.
    fn spawn_fake_provider(
        ctx: &crate::fcast::CompanionContext,
        resources: Vec<(u32, Bytes, u64)>,
    ) {
        use crate::fcast::{CompanionMessage, FeedbackSender, ResourceInfoResponseCell};
        use fcast_protocol::{companion, v4};
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<CompanionMessage>();
        ctx.register_provider(tx);
        std::thread::Builder::new()
            .name("fake-companion".into())
            .spawn(move || {
                let find = |id: u32| resources.iter().find(move |(rid, _, _)| *rid == id);
                while let Some(msg) = rx.blocking_recv() {
                    match msg {
                        CompanionMessage::GetResourceInfo { id, feedback } => {
                            let FeedbackSender::Channel(tx) = feedback;
                            let size = find(id).map(|(_, _, sz)| *sz);
                            let body = v4::MessageBuilder::new()
                                .companion_resource_info_response(0, "audio/mpeg", size)
                                .to_vec();
                            let cell = ResourceInfoResponseCell::new(body, |buf| {
                                v4::flat::root_as_packet(buf)
                                    .unwrap()
                                    .payload_as_companion_resource_info_response()
                                    .unwrap()
                            });
                            let _ = tx.send(cell);
                        }
                        CompanionMessage::GetResource {
                            id,
                            read_head,
                            feedback,
                        } => {
                            let FeedbackSender::Channel(tx) = feedback;
                            let data = find(id).map(|(_, b, _)| b.clone()).unwrap_or_default();
                            let start = read_head.map(|r| r.start()).unwrap_or(0) as usize;
                            let stop_inc = read_head
                                .map(|r| r.stop_inclusive() as usize)
                                .unwrap_or(data.len().saturating_sub(1))
                                .min(data.len().saturating_sub(1));
                            let chunk = if data.is_empty() || start > stop_inc {
                                Vec::new()
                            } else {
                                data[start..=stop_inc].to_vec()
                            };
                            let _ = tx.send(companion::ResourceResponse {
                                request_id: 0,
                                part: 0,
                                total_parts: 1,
                                result: companion::GetResourceResult::Success(bytes::Bytes::from(
                                    chunk,
                                )),
                            });
                        }
                    }
                }
            })
            .unwrap();
    }

    /// Gapless handoff between two `fcomp://` items through a real
    /// `FcastPlaybin`: the next item must play to ITS end, not be cut off at
    /// the previous item's.
    #[test]
    fn gapless_fcomp_next_item_plays_to_its_end() {
        use fcast_protocol::companion;
        use fcastplaybin::{
            AudioSink, FcastPlaybin, MediaInput, MessageHook, PlaybinEvent, Sinks, StartPoint,
        };
        use std::{
            sync::mpsc,
            time::{Duration, Instant},
        };

        crate::gstreamer::init_for_tests();

        // A long enough for its EOS to be paced past the up-front pre-arm; B
        // longer than any plausible overshoot, so a cutoff is unambiguous.
        let a_bytes = make_mp3_bytes(5.0);
        let b_bytes = make_mp3_bytes(4.0);
        let a_len = a_bytes.len() as u64;
        let b_len = b_bytes.len() as u64;

        let ctx = crate::fcast::CompanionContext::new();
        spawn_fake_provider(
            &ctx,
            vec![(0, a_bytes.clone(), a_len), (1, b_bytes.clone(), b_len)],
        );

        let playbin = FcastPlaybin::new(Sinks {
            video: None,
            audio: AudioSink::Factory(Box::new(|| {
                Ok(gst::ElementFactory::make("fakesink")
                    .property("sync", true)
                    .build()?)
            })),
        })
        .unwrap();

        // Provide the fcomp companion context on NeedContext, as `Player::new` does.
        let comp_ctx = crate::fcompsrc::imp::CompContext(ctx.clone());
        let hook: MessageHook = Box::new(move |msg| {
            if let gst::MessageView::NeedContext(nc) = msg.view() {
                let typ = nc.context_type();
                if typ == crate::fcompsrc::imp::FCOMP_CONTEXT {
                    if let Some(el) = msg.src().and_then(|s| s.downcast_ref::<gst::Element>()) {
                        let mut c = gst::Context::new(typ, true);
                        c.get_mut()
                            .unwrap()
                            .structure_mut()
                            .set("context", &comp_ctx);
                        el.set_context(&c);
                    }
                    return true;
                }
            }
            false
        });

        let (tx, rx) = mpsc::channel();
        playbin.set_event_handler(Some(hook), move |event, _generation| match event {
            PlaybinEvent::PreparedActivated => {
                let _ = tx.send("activated");
            }
            PlaybinEvent::EndOfStream => {
                let _ = tx.send("eos");
            }
            _ => {}
        });

        let head = |bytes: Bytes, total: u64| {
            Some(super::PreloadedHead {
                bytes,
                total: Some(total),
            })
        };
        let a_src = super::build_uri_source_with_head(
            &companion::create_url(0, 0),
            None,
            head(a_bytes, a_len),
        )
        .unwrap();
        let b_src = super::build_uri_source_with_head(
            &companion::create_url(0, 1),
            None,
            head(b_bytes, b_len),
        )
        .unwrap();

        playbin
            .load(MediaInput::Element(a_src), StartPoint::Live)
            .unwrap();
        // Pre-arm up front so `pending` is set before A's EOS reaches the hold.
        playbin.prepare_next_async(MediaInput::Element(b_src));
        let t0 = Instant::now();
        playbin.play().unwrap();

        let mut activated = false;
        let eos_elapsed = loop {
            match rx.recv_timeout(Duration::from_secs(25)) {
                Ok("activated") => activated = true,
                Ok("eos") => break Some(t0.elapsed()),
                _ => break None,
            }
        };
        let _ = playbin.stop();

        let eos_elapsed = eos_elapsed.expect("pipeline never reached EOS (wedged)");
        assert!(
            activated,
            "the prepared fcomp item never activated (handoff missed)"
        );
        // A (5s) then B's full 4s ~= 9s; a cutoff ends near A's 5s instead.
        assert!(
            eos_elapsed >= Duration::from_millis(7500),
            "playback ended after {eos_elapsed:?}, expected ~9s (A+B): the fcomp \
             next item was cut off at the previous item's end",
        );
    }

    /// Gapless fcomp handoff with a MID-playback pre-arm and a PACED outgoing
    /// EOS (its decoded EOS reaches the output-side hold AFTER the swap): the
    /// next item must still play to its end.
    #[test]
    fn gapless_fcomp_survives_a_midplayback_prearm() {
        use fcast_protocol::companion;
        use fcastplaybin::{
            AudioSink, FcastPlaybin, MediaInput, MessageHook, PlaybinEvent, Sinks, StartPoint,
        };
        use std::{
            sync::mpsc,
            time::{Duration, Instant},
        };

        crate::gstreamer::init_for_tests();

        let a_bytes = make_mp3_bytes(5.0);
        let b_bytes = make_mp3_bytes(4.0);
        let a_len = a_bytes.len() as u64;
        let b_len = b_bytes.len() as u64;

        let ctx = crate::fcast::CompanionContext::new();
        spawn_fake_provider(
            &ctx,
            vec![(0, a_bytes.clone(), a_len), (1, b_bytes.clone(), b_len)],
        );

        let playbin = FcastPlaybin::new(Sinks {
            video: None,
            audio: AudioSink::Factory(Box::new(|| {
                Ok(gst::ElementFactory::make("fakesink")
                    .property("sync", true)
                    .build()?)
            })),
        })
        .unwrap();
        // Audio-only, so fcastplaybin's decoupling queue is shallow (the deep
        // queue is video-only): the outgoing EOS reaches the gapless hold near
        // the sink boundary, so this mid-playback pre-arm still catches it.

        let comp_ctx = crate::fcompsrc::imp::CompContext(ctx.clone());
        let hook: MessageHook = Box::new(move |msg| {
            if let gst::MessageView::NeedContext(nc) = msg.view() {
                let typ = nc.context_type();
                if typ == crate::fcompsrc::imp::FCOMP_CONTEXT {
                    if let Some(el) = msg.src().and_then(|s| s.downcast_ref::<gst::Element>()) {
                        let mut c = gst::Context::new(typ, true);
                        c.get_mut()
                            .unwrap()
                            .structure_mut()
                            .set("context", &comp_ctx);
                        el.set_context(&c);
                    }
                    return true;
                }
            }
            false
        });
        let (tx, rx) = mpsc::channel();
        playbin.set_event_handler(Some(hook), move |event, _g| match event {
            PlaybinEvent::PreparedActivated => {
                let _ = tx.send("activated");
            }
            PlaybinEvent::EndOfStream => {
                let _ = tx.send("eos");
            }
            _ => {}
        });
        let a_src = super::build_uri_source_with_head(
            &companion::create_url(0, 0),
            None,
            Some(super::PreloadedHead {
                bytes: a_bytes,
                total: Some(a_len),
            }),
        )
        .unwrap();
        playbin
            .load(MediaInput::Element(a_src), StartPoint::Live)
            .unwrap();
        let t0 = Instant::now();
        playbin.play().unwrap();

        // Pre-arm MID-playback (2s into A's 5s): the swap and activation land
        // while A's decoded tail is still draining.
        let pb2 = playbin.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(2));
            let b_src = super::build_uri_source_with_head(
                &companion::create_url(0, 1),
                None,
                Some(super::PreloadedHead {
                    bytes: b_bytes,
                    total: Some(b_len),
                }),
            )
            .unwrap();
            pb2.prepare_next_async(MediaInput::Element(b_src));
        });

        let mut activated = false;
        let eos_elapsed = loop {
            match rx.recv_timeout(Duration::from_secs(30)) {
                Ok("activated") => activated = true,
                Ok("eos") => break Some(t0.elapsed()),
                _ => break None,
            }
        };
        let _ = playbin.stop();

        let eos_elapsed = eos_elapsed.expect("pipeline never reached EOS (wedged)");
        assert!(activated, "the prepared fcomp item never activated");
        // A (5s) then B's full 4s ~= 9s; a cutoff ends near A's 5s instead.
        assert!(
            eos_elapsed >= Duration::from_millis(7500),
            "playback ended after {eos_elapsed:?}, expected ~9s (A+B): the next \
             item was cut off at the previous item's end (mid-playback pre-arm)",
        );
    }

    /// A tiny in-memory animated GIF: `frames` full-canvas 16x16 frames of
    /// different shades, 100ms delay each.
    fn make_gif(frames: u32) -> Vec<u8> {
        use image::codecs::gif::{GifEncoder, Repeat};
        let mut out = Vec::new();
        {
            let mut enc = GifEncoder::new_with_speed(&mut out, 10);
            enc.set_repeat(Repeat::Infinite).unwrap();
            for i in 0..frames {
                let shade = (i * 40 % 256) as u8;
                let img = image::RgbaImage::from_pixel(
                    16,
                    16,
                    image::Rgba([shade, 255 - shade, 128, 255]),
                );
                let frame =
                    image::Frame::from_parts(img, 0, 0, image::Delay::from_numer_denom_ms(100, 1));
                enc.encode_frame(frame).unwrap();
            }
        }
        out
    }

    /// A deterministic pseudorandom byte pattern (xorshift, so no rng
    /// dependency).
    fn pseudorandom_bytes(len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        while out.len() < len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.extend_from_slice(&state.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    /// A GIF served from `build_bytes_source` must feed typefind + parsebin +
    /// decodebin3 exactly like a file source, yielding decoded RGBA samples.
    #[test]
    fn bytes_source_decodes_gif_through_decodebin3() {
        init();

        let gif = Bytes::from(make_gif(4));
        let source = super::build_bytes_source(gif).expect("build bytes source");

        let pipeline = gst::Pipeline::new();
        let db3 = gst::ElementFactory::make("decodebin3").build().unwrap();
        let appsink = gst_app::AppSink::builder()
            .caps(
                &gst::Caps::builder("video/x-raw")
                    .field("format", "RGBA")
                    .build(),
            )
            // Unsynced: samples arrive as fast as they decode.
            .sync(false)
            .build();

        pipeline
            .add_many([&source, &db3, appsink.upcast_ref::<gst::Element>()])
            .unwrap();

        // The bin's parsed src pad appears dynamically, so link on pad-added.
        let db3_weak = db3.downgrade();
        source.connect_pad_added(move |_, pad| {
            let Some(db3) = db3_weak.upgrade() else {
                return;
            };
            let sink = db3.request_pad_simple("sink_%u").unwrap();
            pad.link(&sink).unwrap();
        });
        let appsink_weak = appsink.downgrade();
        db3.connect_pad_added(move |_, pad| {
            if pad.direction() != gst::PadDirection::Src {
                return;
            }
            let Some(appsink) = appsink_weak.upgrade() else {
                return;
            };
            let sink = appsink.static_pad("sink").unwrap();
            if sink.is_linked() {
                return;
            }
            pad.link(&sink).unwrap();
        });

        pipeline.set_state(gst::State::Playing).unwrap();

        let sample = appsink
            .try_pull_sample(gst::ClockTime::from_seconds(10))
            .expect("decoded RGBA sample from the bytes source");
        let s = sample.caps().unwrap().structure(0).unwrap();
        assert_eq!(s.get::<i32>("width").unwrap(), 16);
        assert_eq!(s.get::<i32>("height").unwrap(), 16);
        let buffer = sample.buffer().unwrap();
        let map = buffer.map_readable().unwrap();
        assert_eq!(map.size(), 16 * 16 * 4);

        pipeline.set_state(gst::State::Null).unwrap();
    }

    /// Byte-accurate sequential readback through the raw appsrc: the pulled
    /// buffers concatenate back to the input, with monotonic gap-free offsets.
    #[test]
    fn bytes_appsrc_reads_back_verbatim() {
        init();

        let data = pseudorandom_bytes(3 * 1024 * 1024 + 777);
        let appsrc = super::build_bytes_appsrc(Bytes::from(data.clone()));

        let pipeline = gst::Pipeline::new();
        let appsink = gst_app::AppSink::builder().sync(false).build();
        pipeline
            .add_many([appsrc.upcast_ref::<gst::Element>(), appsink.upcast_ref()])
            .unwrap();
        gst::Element::link_many([appsrc.upcast_ref::<gst::Element>(), appsink.upcast_ref()])
            .unwrap();

        pipeline.set_state(gst::State::Playing).unwrap();

        let mut readback: Vec<u8> = Vec::with_capacity(data.len());
        let mut expected_offset = 0u64;
        loop {
            match appsink.try_pull_sample(gst::ClockTime::from_seconds(10)) {
                Some(sample) => {
                    let buffer = sample.buffer().unwrap();
                    assert_eq!(
                        buffer.offset(),
                        expected_offset,
                        "buffer offset must equal the running read position"
                    );
                    let map = buffer.map_readable().unwrap();
                    expected_offset += map.size() as u64;
                    assert_eq!(
                        buffer.offset_end(),
                        expected_offset,
                        "offset_end must mark the next read position"
                    );
                    readback.extend_from_slice(map.as_slice());
                }
                None => {
                    assert!(appsink.is_eos(), "readback stalled before EOS");
                    break;
                }
            }
            if appsink.is_eos() {
                break;
            }
        }

        assert_eq!(readback.len(), data.len(), "read back a different length");
        assert!(readback == data, "read-back bytes differ from the input");
        assert!(
            appsink.is_eos(),
            "the bytes source must signal EOS at the end"
        );

        pipeline.set_state(gst::State::Null).unwrap();
    }
}
