use std::collections::HashMap;

use anyhow::{Context, Result};
use gst::prelude::*;
use tracing::warn;

use crate::user_agent;

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
    let usb = gst::ElementFactory::make("urisourcebin")
        .property("uri", uri)
        .property("parse-streams", true)
        .property("use-buffering", true)
        .build()
        .context("creating urisourcebin")?;
    if let Some(bin) = usb.downcast_ref::<gst::Bin>() {
        bin.connect_deep_element_added(move |_, _, elem| {
            if elem.factory().map(|f| f.name()).as_deref() == Some("fcasthttpsrc") {
                configure_http_source(elem, headers.as_ref());
            }
        });
    }
    Ok(usb)
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
