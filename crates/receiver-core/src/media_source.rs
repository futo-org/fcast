use std::{collections::HashMap, sync::Arc};

use anyhow::{Context, Result};
use bytes::Bytes;
use gst::prelude::*;
use parking_lot::Mutex;
use tracing::warn;

use crate::user_agent;

/// How much of the cached bytes each `need-data` pull hands downstream. Big
/// enough to keep the demuxer fed without thrashing the callback, small
/// enough that a seek lands promptly.
const BYTES_CHUNK: u64 = 256 * 1024;

/// Apply request headers + a browser user-agent to an `fcasthttpsrc`. Shared
/// by the playbin3 element-setup hook and the fcast per-load source builder.
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

/// Build a urisourcebin for an HTTP/file/DASH/HLS/`data:` URI, wired to apply
/// `headers` to its `fcasthttpsrc` as that element is created, per-load,
/// scoped to THIS urisourcebin, so there is no global header side channel.
/// urisourcebin parses its streams (`parse-streams`), so its src pads feed
/// decodebin3 directly.
pub fn build_uri_source(
    uri: &str,
    headers: Option<HashMap<String, String>>,
) -> Result<gst::Element> {
    build_uri_source_with_head(uri, headers, None)
}

/// A prefetched head of the resource (the queue cache's partial entry),
/// injected into the per-load source element so playback starts from memory
/// and only the remainder streams.
pub struct PreloadedHead {
    pub bytes: Bytes,
    /// Total resource size. Required for the http source (it must know the
    /// size before its first request); fcomp learns its size itself.
    pub total: Option<u64>,
}

/// `build_uri_source` plus an optional prefetched head handed to the source
/// element that urisourcebin creates (fcasthttpsrc or fcompsrc).
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
                    // http needs the total up front, without it the head is
                    // unusable here and the source just streams normally.
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

/// Build a source that serves an already-prefetched queue item straight from
/// memory, so a cached item plays without hitting the network again. The
/// `bytes` (a refcounted `bytes::Bytes`, cheap to hold) are fed through a
/// seekable `appsrc` that answers byte-duration and seek queries like an
/// http source, then parsebin-wrapped so fcastplaybin receives PARSED streams
/// exactly as urisourcebin's `parse-streams=true` would produce.
pub fn build_bytes_source(bytes: Bytes) -> Result<gst::Element> {
    let appsrc = build_bytes_appsrc(bytes);
    wrap_with_parsebin(appsrc.upcast(), "fcast-bytes-source")
}

/// Construct just the seekable `appsrc` over `bytes`, without the
/// parsebin wrap. Split out so it can be unit-tested for byte-accurate
/// readback independent of typefind/parsebin (which cannot type arbitrary
/// bytes). `build_bytes_source` is the only production caller and always
/// wraps the result.
///
/// The source is a seekable BYTES source sized to the buffer, so downstream
/// duration-in-bytes and seek queries behave like a file. `need-data` pushes
/// the next `BYTES_CHUNK` slice from the current offset (a zero-copy
/// refcounted `Bytes::slice`, never a copy of the whole buffer) and signals
/// end-of-stream once the offset reaches the end. `seek-data` just moves the
/// offset. Both callbacks share the offset behind a `parking_lot::Mutex`.
fn build_bytes_appsrc(bytes: Bytes) -> gst_app::AppSrc {
    let len = bytes.len() as u64;
    // Shared read cursor. seek-data moves it, need-data advances it.
    let offset = Arc::new(Mutex::new(0u64));

    let appsrc = gst_app::AppSrc::builder()
        .format(gst::Format::Bytes)
        // Seekable, not RandomAccess: seekable is push mode with seek-data,
        // matching how every network source here behaves. RandomAccess
        // advertises pull scheduling, which collides with the push-based
        // feeding when parsebin's typefind activates the chain in pull mode.
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
                    // At or past the end (including after a seek beyond it):
                    // signal EOS and stop. A later seek back in-range
                    // resumes cleanly on the next need-data.
                    if *pos >= len {
                        let _ = appsrc.end_of_stream();
                        return;
                    }
                    let start = *pos;
                    let end = (start + BYTES_CHUNK).min(len);
                    // Zero-copy: slice() bumps the refcount, from_slice wraps
                    // the Bytes as the buffer's memory owner without copying.
                    let chunk = bytes.slice(start as usize..end as usize);
                    let mut buffer = gst::Buffer::from_slice(chunk);
                    {
                        let buffer = buffer.get_mut().unwrap();
                        buffer.set_offset(start);
                        buffer.set_offset_end(end);
                    }
                    *pos = end;
                    // Release the lock before pushing so a re-entrant
                    // need-data from downstream cannot deadlock.
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
                    // Accept any offset. An out-of-range value simply makes
                    // the next need-data emit EOS, no panic and no busy loop.
                    *offset.lock() = new_offset;
                    true
                }
            })
            .build(),
    );

    appsrc
}

/// Build the WHEP source directly (no `fcastwhep://` urisourcebin dispatch).
/// `fcastwhepsrcbin` is a URIHandler keyed on the `fcastwhep://` scheme, its
/// endpoint is set directly here. It emits RTP, so it is parsebin-wrapped.
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

/// Build the fwebrtc source directly, with the signalling channel handed over
/// as a typed property (a live object that cannot travel through a URI, this
/// is why fwebrtc MUST be a directly-constructed element). Emits RTP, so it is
/// parsebin-wrapped.
pub fn build_fwebrtc_source<C: Into<gst::glib::Value>>(channel: C) -> Result<gst::Element> {
    let src = gst::ElementFactory::make("fwebrtcsrc")
        .build()
        .context("creating fwebrtcsrc")?;
    src.set_property_from_value("signalling-channel", &channel.into());
    wrap_with_parsebin(src, "fcast-fwebrtc-source")
}

/// Build the AirPlay mirror source directly (no `airplay://` urisourcebin
/// dispatch). `airplaysrc` emits encoded H.264/AAC, so decodebin3 decodes it
/// directly, no parsebin wrap needed.
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
/// pads carry PARSED streams, mirroring urisourcebin's `parse-streams`: for
/// each dynamic source pad, spin up a `parsebin` and ghost its parsed output
/// out of the returned bin. Used for the WHEP/fwebrtc RTP sources, which
/// today reach decodebin3 through urisourcebin's internal parsebin.
///
/// `name` is a PREFIX: the bin gets a unique suffix. Two of these can share
/// one pipeline (a gapless pre-arm adds the next item's source while the
/// current one still plays), and GStreamer rejects duplicate child names.
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

/// Add a `parsebin` for one source pad and ghost its parsed output pads out of
/// `bin` (so the enclosing pipeline links them to decodebin3).
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
            // Test A decodes a GIF, which decodebin3 autoplugs to fimagedec.
            crate::imagedec::plugin_init().unwrap();
        });
    }

    // --- gapless fcomp handoff repro (the musikkspiller cutoff) --------------

    /// Encode `seconds` of silence to MP3 (audio/mpeg, the fcomp container) at
    /// a fixed CBR `bitrate` (kbps) and return the bytes, so a fake companion
    /// provider can serve them.
    fn make_mp3_bytes(seconds: f64) -> Bytes {
        make_mp3_bytes_at(seconds, 128)
    }

    fn make_mp3_bytes_at(seconds: f64, bitrate_kbps: i32) -> Bytes {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("fcomp-gapless-{}-{n}.mp3", std::process::id()));
        // audiotestsrc defaults: 44100 Hz, 1024 samples/buffer.
        let num_buffers = (seconds * 44100.0 / 1024.0).round() as i32;
        let src = gst::ElementFactory::make("audiotestsrc")
            .property("num-buffers", num_buffers)
            .property("is-live", false)
            .property_from_str("wave", "silence")
            .build()
            .unwrap();
        let conv = gst::ElementFactory::make("audioconvert").build().unwrap();
        let enc = gst::ElementFactory::make("lamemp3enc")
            .property_from_str("target", "bitrate")
            .property("cbr", true)
            .property("bitrate", bitrate_kbps)
            .build()
            .unwrap();
        let sink = gst::ElementFactory::make("filesink")
            .property("location", path.to_str().unwrap())
            .build()
            .unwrap();
        let pipeline = gst::Pipeline::new();
        pipeline.add_many([&src, &conv, &enc, &sink]).unwrap();
        gst::Element::link_many([&src, &conv, &enc, &sink]).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let bus = pipeline.bus().unwrap();
        while let Some(msg) = bus.timed_pop(gst::ClockTime::from_seconds(10)) {
            match msg.view() {
                gst::MessageView::Eos(_) => break,
                gst::MessageView::Error(e) => panic!("mp3 encode failed: {e:?}"),
                _ => {}
            }
        }
        pipeline.set_state(gst::State::Null).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        Bytes::from(bytes)
    }

    /// Register a fake companion provider on `ctx` serving `(resource_id,
    /// bytes, reported_size)`. A `reported_size` larger than `bytes.len()`
    /// reproduces the field's byte-size overshoot (declared duration > real
    /// audio); equal is the honest case.
    fn spawn_fake_provider(ctx: &crate::fcast::CompanionContext, resources: Vec<(u32, Bytes, u64)>) {
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
                                result: companion::GetResourceResult::Success(chunk),
                            });
                        }
                    }
                }
            })
            .unwrap();
    }

    /// Gapless handoff between two `fcomp://` items (fcompsrc + preloaded head
    /// via `build_uri_source_with_head`, the real field source) through a real
    /// `FcastPlaybin`: the next item must play to ITS end, not be cut off at
    /// the previous item's. Guards the fcomp gapless path end to end.
    ///
    /// NOTE: this reaches the exact source topology of the musikkspiller cutoff
    /// but does NOT reproduce it (B plays fully) for any synthesizable A --
    /// honest sizing, zero-padded byte extent, or a low-bitrate lead-frame
    /// duration overshoot all pass. streamsynchronizer uses A's DECODED extent
    /// for the group start, so "declared > decoded" alone does not cut B. The
    /// field trigger needs a condition this harness lacks (see the memory note
    /// gapless-fcompsrc-cutoff); feed the real casting bytes to `a_bytes`/
    /// `b_bytes` below to reproduce deterministically.
    #[test]
    fn gapless_fcomp_next_item_plays_to_its_end() {
        use fcast_protocol::companion;
        use fcastplaybin::{
            AudioSink, FcastPlaybin, MediaInput, MessageHook, PlaybinEvent, Sinks, StartPoint,
        };
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        crate::gstreamer::init_for_tests();
        // init_for_tests only calls gst::init; fcompsrc is a receiver plugin.
        // Ignore a duplicate-registration error when run beside other tests.
        let _ = crate::fcompsrc::plugin_init();

        // A long enough for its EOS to be paced past the up-front pre-arm; B
        // longer than any plausible overshoot so a cutoff would be unambiguous.
        // (Swap these for real casting bytes to chase the field cutoff.)
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

        // Provide the fcomp companion context on NeedContext, exactly as
        // `Player::new`'s message hook does.
        let comp_ctx = crate::fcompsrc::imp::CompContext(ctx.clone());
        let hook: MessageHook = Box::new(move |msg| {
            if let gst::MessageView::NeedContext(nc) = msg.view() {
                let typ = nc.context_type();
                if typ == crate::fcompsrc::imp::FCOMP_CONTEXT {
                    if let Some(el) = msg
                        .src()
                        .and_then(|s| s.downcast_ref::<gst::Element>())
                    {
                        let mut c = gst::Context::new(typ, true);
                        c.get_mut().unwrap().structure_mut().set("context", &comp_ctx);
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
        // Pre-arm up front so `pending` is set before A's EOS reaches the hold
        // (the field pre-arms tens of seconds early).
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
        assert!(activated, "the prepared fcomp item never activated (handoff missed)");
        // A (5s) then B's full 4s ~= 9s. A cutoff would end playback near A's
        // 5s instead.
        assert!(
            eos_elapsed >= Duration::from_millis(7500),
            "playback ended after {eos_elapsed:?}, expected ~9s (A+B): the fcomp \
             next item was cut off at the previous item's end",
        );
    }

    /// Gapless fcomp handoff with a MID-playback pre-arm (the field's timing)
    /// and a PACED outgoing EOS: the 30s audio decoupling queue is shrunk so a
    /// short item behaves like a real long track (its decoded EOS reaches the
    /// output-side hold AFTER the swap, not buffered and released early). The
    /// next item must play to its end. Guards the mid-playback gapless path.
    ///
    /// NOTE: here the outgoing EOS lands while `pending` is still set (the
    /// output-side activation has not run yet), so the hold drops it on
    /// `pending` alone and this passes with OR without the retire-at-swap
    /// hardening. The field's narrower window (an input-side STREAMS_SELECTED
    /// clearing `pending` BEFORE the paced EOS, which then needs the
    /// retired-group check) needs decodebin3 to post that early selection,
    /// which a synthetic single-stream source does not. See the memory note
    /// gapless-fcompsrc-cutoff.
    #[test]
    fn gapless_fcomp_survives_a_midplayback_prearm() {
        use fcast_protocol::companion;
        use fcastplaybin::{
            AudioSink, FcastPlaybin, MediaInput, MessageHook, PlaybinEvent, Sinks, StartPoint,
        };
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        crate::gstreamer::init_for_tests();
        let _ = crate::fcompsrc::plugin_init();

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
        // Audio-only, so fcastplaybin's decoupling queue is shallow (the
        // deep queue is video-only): the outgoing item's EOS reaches the
        // gapless hold near the sink boundary, so this mid-playback pre-arm
        // still catches it. With the old unconditional 30s queue the EOS
        // decoupled ~30s early and this failed (B ended at A's ~5s).

        let comp_ctx = crate::fcompsrc::imp::CompContext(ctx.clone());
        let hook: MessageHook = Box::new(move |msg| {
            if let gst::MessageView::NeedContext(nc) = msg.view() {
                let typ = nc.context_type();
                if typ == crate::fcompsrc::imp::FCOMP_CONTEXT {
                    if let Some(el) = msg.src().and_then(|s| s.downcast_ref::<gst::Element>()) {
                        let mut c = gst::Context::new(typ, true);
                        c.get_mut().unwrap().structure_mut().set("context", &comp_ctx);
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

        // Pre-arm MID-playback (2s into A's 5s), the field ordering: the swap
        // and activation land while A's decoded tail is still draining.
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
        // A (5s) then B's full 4s ~= 9s. The bug cuts B when A's paced EOS
        // reaches the (already-activated) hold, ending playback near A's 5s.
        assert!(
            eos_elapsed >= Duration::from_millis(7500),
            "playback ended after {eos_elapsed:?}, expected ~9s (A+B): the next \
             item was cut off at the previous item's end (mid-playback pre-arm)",
        );
    }

    /// A tiny in-memory animated GIF (mirrors imagedec's test helper): four
    /// full-canvas 16x16 frames of different shades, 100ms delay each. Gives
    /// Test A real container bytes to typefind, parse, and decode.
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

    /// A few MB of a deterministic pseudorandom byte pattern (a simple
    /// xorshift so the test needs no rng dependency). Used to prove
    /// byte-accurate readback through the appsrc.
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

    /// Test A: a GIF served from `build_bytes_source` must feed
    /// typefind + parsebin + decodebin3 exactly like a file source, yielding
    /// decoded RGBA video samples. Proves the appsrc-in-a-parsebin-bin is a
    /// drop-in seekable source for the real decode path.
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

        // The bytes-source bin exposes its parsed src pad dynamically (through
        // parsebin's ghost pad), so link into decodebin3 on pad-added.
        let db3_weak = db3.downgrade();
        source.connect_pad_added(move |_, pad| {
            let Some(db3) = db3_weak.upgrade() else {
                return;
            };
            let sink = db3.request_pad_simple("sink_%u").unwrap();
            pad.link(&sink).unwrap();
        });
        // decodebin3's decoded output is likewise dynamic.
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

        // A decoded RGBA sample at the GIF's dimensions proves the whole
        // typefind -> parsebin -> decodebin3 -> fimagedec chain ran off the
        // in-memory appsrc.
        let sample = appsink
            .try_pull_sample(gst::ClockTime::from_seconds(10))
            .expect("decoded RGBA sample from the bytes source");
        let s = sample.caps().unwrap().structure(0).unwrap();
        assert_eq!(s.get::<i32>("width").unwrap(), 16);
        assert_eq!(s.get::<i32>("height").unwrap(), 16);
        let buffer = sample.buffer().unwrap();
        let map = buffer.map_readable().unwrap();
        // RGBA 16x16 = 1024 bytes of decoded video.
        assert_eq!(map.size(), 16 * 16 * 4);

        pipeline.set_state(gst::State::Null).unwrap();
    }

    /// Test B: byte-accurate sequential readback. Drive the raw appsrc helper
    /// through `appsrc ! appsink`, pull every buffer to EOS, and assert the
    /// concatenation equals the input and buffer offsets are monotonic and
    /// gap-free. This validates the need-data chunking and the size/EOS
    /// bookkeeping without typefind (which cannot type random bytes).
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
                    // Offsets are monotonic and contiguous (no gaps, no
                    // rewind) across the sequential read.
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
                    // No more samples within the timeout: EOS must have
                    // arrived by now.
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
