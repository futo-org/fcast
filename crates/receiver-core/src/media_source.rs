use std::collections::HashMap;
use std::sync::Arc;

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
fn wrap_with_parsebin(source: gst::Element, name: &str) -> Result<gst::Element> {
    let bin = gst::Bin::builder().name(name).build();
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
                let frame = image::Frame::from_parts(
                    img,
                    0,
                    0,
                    image::Delay::from_numer_denom_ms(100, 1),
                );
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
            let Some(db3) = db3_weak.upgrade() else { return };
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
        assert!(appsink.is_eos(), "the bytes source must signal EOS at the end");

        pipeline.set_state(gst::State::Null).unwrap();
    }
}
